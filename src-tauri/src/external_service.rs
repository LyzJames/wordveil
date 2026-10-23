//! Trusted UI boundary for M2-B. Immutable previews are single-use, expire in
//! ten minutes and bind the exact skill input to exactly one confirmed root.
use super::*;
use crate::{generation::credentials::CredentialStore, network, mcp, public_knowledge::Package};

impl CoreService {
    pub fn public_mcp_start(self:&Arc<Self>)->Result<Value>{
        let handle=self.vault()?; drop(self.lock_vault(&handle)?);
        let mut running=self.services.public_mcp.lock().map_err(|_|Error::Integrity)?;
        if let Some(process)=running.as_mut(){if process.running(){return Ok(json!({"connection_id":process.connection_id,"endpoint":process.endpoint,"status":"already_running"}));}}
        *running=None;
        let path=if self.services.layout.production{self.services.layout.install_root.join("lvjin-public-mcp.exe")}else{self.services.layout.install_root.join("build/cargo-ascii/debug/lvjin-public-mcp.exe")};
        if !path.is_file(){return Err(Error::NeedsInput("public_mcp_binary"));}
        if self.services.layout.production{
            let installation=crate::installation::Installation::load(&self.services.layout.install_root)?;
            let file=installation.runtime_files.iter().find(|file|file.path=="lvjin-public-mcp.exe").ok_or(Error::Integrity)?;
            crate::worker_process::verify_files(&self.services.layout.install_root,std::slice::from_ref(file))?;
        }
        let token=Zeroizing::new(format!("{}{}",uuid::Uuid::new_v4().simple(),uuid::Uuid::new_v4().simple()));
        let mut process=crate::public_mcp_server::Managed::launch(&path,&token)?;
        let draft=self.external_draft("mcp",json!({"name":"律金公共法律金融（本机只读）","endpoint":process.endpoint,"timeout_s":30,"proxy":"none"}),Some(token))?;
        let envelope=self.external_settings(&draft)?;
        if envelope["status"]!="completed"{return Err(Error::Integrity);}
        let result_ref=envelope["data_ref"].as_str().ok_or(Error::Integrity)?;
        let payload=self.lock_vault(&handle)?.get_result(result_ref)?.2;
        process.connection_id=payload["result"]["connection_id"].as_str().ok_or(Error::Integrity)?.to_owned();
        self.lock_vault(&handle)?.set_setting(&format!("managed-mcp:{}",process.connection_id),"1")?;
        let summary=json!({"connection_id":process.connection_id,"endpoint":process.endpoint,"status":"running"});
        *running=Some(process);
        Ok(summary)
    }
    pub fn public_mcp_stop(&self)->Result<()>{
        let process=self.services.public_mcp.lock().map_err(|_|Error::Integrity)?.take();
        if let Some(process)=process{
            let id=process.connection_id.clone();drop(process);
            let handle=self.vault()?;let vault=self.lock_vault(&handle)?;
            let credential:Option<String>=vault.db().query_row("SELECT credential FROM external_connections WHERE id=?",[&id],|r|r.get(0))?;
            vault.db().execute("UPDATE external_connections SET enabled=0,credential=NULL WHERE id=?",[&id])?;
            vault.db().execute("UPDATE external_bindings SET active=0 WHERE connection_id=?",[&id])?;
            drop(vault);if let Some(id)=credential{self.credential_store()?.remove(&id)?;}
        }
        Ok(())
    }
    pub fn external_draft(&self, kind: &str, value: Value, key: Option<Zeroizing<String>>) -> Result<String> {
        if !matches!(kind, "source" | "mcp" | "binding" | "disable_source" | "disable_mcp" | "disable_package") || value.to_string().len() > 32768 { return Err(Error::Contract); }
        let credential = if let Some(key) = key {
            if kind != "mcp" || key.is_empty() || key.len() > 4096 || key.chars().any(char::is_control) { return Err(Error::Contract); }
            let id = CredentialStore::new_id(); self.credential_store()?.put(&id, &key)?; Some(id)
        } else { None };
        let handle = self.vault()?;
        let vault = self.lock_vault(&handle)?;
        let id = vault.register("ConnectionDraftRef", "m2-b", None)?;
        let profile = json!({"external_kind":kind,"value":value});
        vault.db().execute("INSERT INTO connection_drafts(id,profile,credential,created_at) VALUES(?,?,?,?)", params![id, profile.to_string(), credential, now_rfc3339()])?;
        Ok(id)
    }
    pub fn external_settings(self: &Arc<Self>, draft: &str) -> Result<Value> {
        self.run_skill("connection-settings", json!({"operation":"save","connection_draft_id":draft}), "protected")
    }
    pub fn external_draft_preview(&self,id:&str)->Result<Value>{
        let handle=self.vault()?;let vault=self.lock_vault(&handle)?;
        vault.expect_kind(id,&["ConnectionDraftRef"])?;
        let text:String=vault.db().query_row("SELECT profile FROM connection_drafts WHERE id=? AND consumed=0",[id],|r|r.get(0))?;
        let mut profile:Value=serde_json::from_str(&text)?;
        if profile["external_kind"]=="binding"{
            let catalog=profile["value"]["catalog_ref"].as_str().ok_or(Error::Contract)?;
            vault.expect_kind(catalog,&["ToolCatalogRef"])?;
            let payload=vault.get_result(catalog)?.2;
            let name=profile["value"]["tool_name"].as_str().ok_or(Error::Contract)?;
            let tool=payload["exchange"]["result"]["tools"].as_array().ok_or(Error::Contract)?.iter().find(|t|t["name"]==name).cloned().ok_or(Error::Denied)?;
            profile["tool_review"]=tool;
            profile["connection_id"]=payload["connection_id"].clone();
        }
        Ok(profile)
    }
    pub fn external_draft_discard(&self,id:&str)->Result<()>{
        let handle=self.vault()?;let vault=self.lock_vault(&handle)?;
        let credential:Option<String>=vault.db().query_row("SELECT credential FROM connection_drafts WHERE id=? AND consumed=0",[id],|r|r.get(0)).optional()?.flatten();
        vault.db().execute("UPDATE connection_drafts SET consumed=1 WHERE id=?",[id])?;
        drop(vault);
        if let Some(id)=credential{self.credential_store()?.remove(&id)?;}
        Ok(())
    }
    pub fn external_overview(&self) -> Result<Value> {
        let handle = self.vault()?;
        let vault = self.lock_vault(&handle)?;
        let mut stmt = vault.db().prepare("SELECT s.id,c.name,s.domains,s.enabled FROM network_sources s LEFT JOIN connections c ON c.id=s.connection_id ORDER BY s.created_at")?;
        let sources = stmt.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,Option<String>>(1)?,"domains":serde_json::from_str::<Value>(&r.get::<_,String>(2)?).unwrap_or(Value::Null),"enabled":r.get::<_,i64>(3)? != 0})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let mut stmt = vault.db().prepare("SELECT id,name,endpoint,enabled FROM external_connections ORDER BY created_at")?;
        let connections = stmt.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"endpoint":r.get::<_,String>(2)?,"enabled":r.get::<_,i64>(3)? != 0})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let mut stmt = vault.db().prepare("SELECT b.id,b.connection_id,b.name,b.descriptor,b.schema_hash,b.active FROM external_bindings b JOIN external_connections c ON c.id=b.connection_id WHERE c.enabled=1 AND b.name<>'__catalog__' ORDER BY b.created_at")?;
        let bindings = stmt.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"connection_id":r.get::<_,String>(1)?,"name":r.get::<_,String>(2)?,"descriptor":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap_or(Value::Null),"schema_hash":r.get::<_,String>(4)?,"active":r.get::<_,i64>(5)? != 0})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let mut stmt = vault.db().prepare("SELECT id,package_id,version,sha256,state,metadata,created_at FROM knowledge_packages ORDER BY created_at DESC")?;
        let packages = stmt.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"package_id":r.get::<_,String>(1)?,"version":r.get::<_,String>(2)?,"sha256":r.get::<_,String>(3)?,"state":r.get::<_,String>(4)?,"metadata":serde_json::from_str::<Value>(&r.get::<_,String>(5)?).unwrap_or(Value::Null),"created_at":r.get::<_,String>(6)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let mut stmt = vault.db().prepare("SELECT host,operation,status,error,started_at FROM network_requests ORDER BY started_at DESC LIMIT 100")?;
        let requests = stmt.query_map([], |r| Ok(json!({"host":r.get::<_,String>(0)?,"operation":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"error":r.get::<_,Option<String>>(3)?,"started_at":r.get::<_,String>(4)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        Ok(json!({"sources":sources,"connections":connections,"bindings":bindings,"packages":packages,"requests":requests}))
    }

    fn external_action(&self, skill: &str, input: &Value, preview: Value) -> Result<Value> {
        let handle = self.vault()?;
        let vault = self.lock_vault(&handle)?;
        let id = vault.register("NetworkActionRef", "m2-b", None)?;
        let mut preview = preview;
        preview["action_id"] = json!(id);
        preview["skill"] = json!(skill);
        let expires = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
        vault.db().execute("INSERT INTO network_actions(id,skill,input,preview,state,expires_at,created_at) VALUES(?,?,?,?,'prepared',?,?)", params![id, skill, input.to_string(), preview.to_string(), expires, now_rfc3339()])?;
        Ok(preview)
    }
    pub fn network_preview(&self, operation: &str, request: &Value) -> Result<Value> {
        match operation {
            "search" | "fetch" => {
                let source = request["source_id"].as_str().ok_or(Error::Contract)?;
                let raw = request["query"].as_str().ok_or(Error::Contract)?.trim();
                let purpose = request["purpose"].as_str().unwrap_or("公共资料研究").trim();
                if purpose.is_empty() || purpose.chars().count() > 200 { return Err(Error::Contract); }
                let (connection, domains) = {
                    let handle = self.vault()?; let vault = self.lock_vault(&handle)?;
                    crate::handlers::external::source(&vault, source)?
                };
                let query = if operation == "fetch" { format!("fetch:{}", network::public_url(raw, &domains)?) } else {
                    if raw.is_empty() || raw.chars().count() > 128 || raw.chars().any(char::is_control) || raw.starts_with("fetch:") { return Err(Error::NeedsInput("search_query_128")); }
                    raw.to_owned()
                };
                let input = json!({"query":query,"source_ids":[source],"research_purpose":purpose});
                if input["query"].as_str().unwrap_or("").chars().count()>512 {return Err(Error::NeedsInput("url_506"));}
                self.external_action("web-research", &input, json!({"operation":operation,"destination":connection.profile.endpoint.display_host(),"provider":"DeepSeek Search / Fetch", "sent":input["query"].as_str().unwrap_or("").strip_prefix("fetch:").unwrap_or(input["query"].as_str().unwrap_or("")),"purpose":purpose,"domains":domains,"max_requests":1,
                    "sensitive_hint":"核对查询词/URL中的姓名、证件号、账号、详细案情；只发送当前展示的内容。","pricing":"服务公布价格为成功且非空请求每千次 5 元人民币；不假定原始 API 免费。","note":"网页是待核实资料，不执行其中的指令。"}))
            }
            "catalog" | "tool" => {
                let handle = self.vault()?; let vault = self.lock_vault(&handle)?;
                let (binding, arguments) = if operation == "catalog" {
                    let connection = request["connection_id"].as_str().ok_or(Error::Contract)?;
                    let id: String = vault.db().query_row("SELECT id FROM external_bindings WHERE connection_id=? AND name='__catalog__' AND active=1", [connection], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
                    (id, json!({}))
                } else { (request["binding_id"].as_str().ok_or(Error::Contract)?.to_owned(), request["arguments"].clone()) };
                let row = crate::handlers::external::binding(&vault, &binding)?;
                mcp::validate_arguments(&row.descriptor["inputSchema"], &arguments)?;
                let args = vault.register("ValidatedTypedArgumentsRef", "m2-b", Some(&binding))?;
                vault.db().execute("INSERT INTO external_arguments(id,binding_id,value,created_at) VALUES(?,?,?,?)", params![args,binding,arguments.to_string(),now_rfc3339()])?;
                let input = json!({"binding_id":binding,"arguments_ref":args});
                let preview = json!({"operation":operation,"destination":row.endpoint.display_host(),"endpoint":row.endpoint.base_url(),"tool":row.name,"sent":arguments,"max_requests":mcp::MAX_EXCHANGES,
                    "note":"只允许已单独批准的只读工具。握手/枚举与本次调用共用预算；Schema 或服务器版本变化时旧绑定失效。"});
                drop(vault); drop(handle);
                self.external_action("external-tools", &input, preview)
            }
            _ => Err(Error::Contract),
        }
    }
    pub fn action_preview(&self, id: &str) -> Result<Value> {
        let handle = self.vault()?; let vault = self.lock_vault(&handle)?;
        vault.expect_kind(id, &["NetworkActionRef"])?;
        let (preview, state, expires): (String,String,String) = vault.db().query_row("SELECT preview,state,expires_at FROM network_actions WHERE id=?", [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        if state != "prepared" || chrono::DateTime::parse_from_rfc3339(&expires).map_err(|_| Error::Integrity)? <= chrono::Utc::now() { return Err(Error::Denied); }
        Ok(serde_json::from_str(&preview)?)
    }
    fn begin_action(&self, action: &str) -> Result<(String,Value,VaultHandle,crate::runtime::Invocation)> {
        self.action_preview(action)?;
        let (skill, input) = {
            let handle = self.vault()?; let vault = self.lock_vault(&handle)?;
            let (skill, input): (String,String) = vault.db().query_row("SELECT skill,input FROM network_actions WHERE id=? AND state='prepared'", [action], |r| Ok((r.get(0)?,r.get(1)?)))?;
            (skill, serde_json::from_str::<Value>(&input)?)
        };
        let (handle, invocation) = self.begin(&skill, &input, "confirm")?;
        let bound = self.lock_vault(&handle)?.db().execute("UPDATE network_actions SET root=?,state='approved' WHERE id=? AND state='prepared' AND root IS NULL", params![invocation.root_id(), action])?;
        if bound != 1 { return Err(Error::Denied); }
        Ok((skill,input,handle,invocation))
    }
    /// Called only after the native shell confirms `action_preview(id)`.
    pub fn action_start(self: &Arc<Self>, id: &str) -> Result<String> {
        let (skill,input,handle,invocation) = self.begin_action(id)?;
        self.spawn(&skill,input,handle,invocation)
    }
    #[cfg(feature="m0-harness")]
    pub fn harness_run_action(&self, id: &str) -> Result<Value> {
        let (_,input,handle,invocation) = self.begin_action(id)?;
        let cancel = self.session_cancel(&handle)?;
        let sink = |_:Progress| {};
        handlers::execute(&Ctx{vault:&handle,services:&self.services,cancel:&cancel,progress:&sink}, &invocation, &input)
    }
    pub fn action_discard(&self, id: &str) -> Result<()> {
        let handle = self.vault()?; let vault = self.lock_vault(&handle)?;
        vault.expect_kind(id, &["NetworkActionRef"])?;
        vault.db().execute("UPDATE network_actions SET state='revoked' WHERE id=? AND state IN ('prepared','approved')", [id])?;
        Ok(())
    }

    pub fn knowledge_preview(&self, bytes: &[u8], origin: &str) -> Result<Value> {
        let package = Package::parse(bytes)?;
        let canonical = serde_json::to_string(&package)?;
        let hash = sha256_hex(canonical.as_bytes());
        let handle = self.vault()?; let vault = self.lock_vault(&handle)?;
        let prior: Option<(String,String)> = vault.db().query_row("SELECT version,metadata FROM knowledge_packages WHERE package_id=? AND state='active' ORDER BY created_at DESC LIMIT 1", [&package.package_id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let previous = prior.as_ref().map(|(_,m)| serde_json::from_str::<Value>(m)).transpose()?.unwrap_or(json!({"documents":[]}));
        let old_docs = previous["documents"].as_array().cloned().unwrap_or_default();
        let changed = package.documents.iter().filter(|d| !old_docs.iter().any(|old| old["id"] == d.id && old["sha256"] == d.sha256)).map(|d| d.metadata()).collect::<Vec<_>>();
        let removed = old_docs.iter().filter(|old| !package.documents.iter().any(|d| old["id"] == d.id)).cloned().collect::<Vec<_>>();
        let preview = json!({"package":package.summary(),"sha256":hash,"previous_version":prior.map(|p|p.0),"changed":changed,"removed":removed,"origin":origin,"note":"确认后导入公共集合并建立索引；保留旧包版本及历史引用。哈希校验不等同于第三方签名或法律适用性核定。"});
        let source = vault.register("KnowledgeSourceRef", "m2-b", None)?;
        vault.db().execute("INSERT INTO knowledge_sources(id,package,sha256,preview,origin,state,created_at) VALUES(?,?,?,?,?,'previewed',?)", params![source,canonical,hash,preview.to_string(),origin,now_rfc3339()])?;
        drop(vault); drop(handle);
        let input = json!({"source_selection_id":source,"update_mode":"apply"});
        self.external_action("knowledge-update", &input, json!({"operation":"knowledge_apply","destination":"本机加密公共知识集合","sent":preview,"max_requests":0}))
    }
    pub fn builtin_knowledge_preview(&self) -> Result<Value> {
        self.knowledge_preview(crate::public_mcp_server::PACKAGE_BYTES,"bundled-official-texts")
    }
    pub fn web_knowledge_preview(&self,result_ref:&str,item:usize,metadata:Value)->Result<Value>{
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Review{title:String,issuer:String,published_on:String,effective_on:String,version:String,confirmed_official_text:bool}
        let review:Review=serde_json::from_value(metadata)?;
        if !review.confirmed_official_text{return Err(Error::NeedsInput("official_text_rights"));}
        let handle=self.vault()?;let vault=self.lock_vault(&handle)?;
        vault.expect_kind(result_ref,&["WebResearchResultRef"])?;
        let payload=vault.get_result(result_ref)?.2;
        let row=payload["items"].as_array().and_then(|rows|rows.get(item)).ok_or(Error::Denied)?;
        if row["snapshot"]!="fetched-markdown"{return Err(Error::Denied);}
        let text=row["text"].as_str().ok_or(Error::Contract)?;
        if row["sha256"]!=sha256_hex(text.as_bytes()){return Err(Error::Integrity);}
        let url=row["url"].as_str().ok_or(Error::Contract)?;
        network::public_url(url,&network::OFFICIAL_DOMAINS.iter().map(|s|(*s).into()).collect::<Vec<_>>())?;
        let source_id=format!("official-web-{}",&sha256_hex(url.as_bytes())[..24]);
        let today=crate::public_knowledge::mainland_date(chrono::Utc::now())?;
        let package=crate::public_knowledge::Package{schema_version:crate::public_knowledge::FORMAT.into(),package_id:source_id.clone(),version:review.version,title:review.title.clone(),region:"CN-mainland".into(),as_of:today.clone(),
            limitations:vec!["网页精读快照，经用户确认来源、正式文本权利与所填日期后导入；核验日期表示本次用户确认，不是系统自动核定法律效力。".into(),"不含全部地方规定、司法解释或实时市场数据；个案适用仍需核对。".into()],
            documents:vec![crate::public_knowledge::PublicDocument{id:source_id,title:review.title,issuer:review.issuer,source_url:url.into(),published_on:review.published_on,effective_on:review.effective_on,expires_on:None,region:"CN-mainland".into(),status:"in_force_as_of".into(),rights:crate::public_knowledge::RIGHTS.into(),verified_on:today,text:text.into(),sha256:sha256_hex(text.as_bytes())}]};
        let bytes=serde_json::to_vec(&package)?;drop(vault);drop(handle);
        self.knowledge_preview(&bytes,"deepseek-fetch-user-reviewed-official-text")
    }
}
