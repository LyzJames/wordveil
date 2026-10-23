//! All external results remain untrusted data. A root without a matching
//! single-use native approval cannot call these handlers or their adapters.
use super::*;
use crate::{generation::{self, connections, credentials::CredentialStore, transport::{Endpoint, HttpOptions, ProxyMode}}, mcp, network, store::sha256_hex, vault::now_rfc3339};
use rusqlite::{OptionalExtension, params};
use std::time::Duration;

pub fn credential_store(ctx: &Ctx) -> Result<CredentialStore> {
    let vault=ctx.lock()?;
    CredentialStore::for_vault(&ctx.services.layout.data_root, &vault.id, &vault.keys.credential)
}

pub fn check_search_connection(connection:&connections::Connection)->Result<()>{
    let endpoint=&connection.profile.endpoint;
    let official=connection.profile.provider==generation::Provider::Deepseek && endpoint.scheme=="https" && endpoint.host=="api.deepseek.com" && endpoint.port.is_none_or(|p|p==443) && matches!(endpoint.base_path.as_str(),""|"/v1");
    if connection.status!="active" || (!official && !(cfg!(feature="m0-harness")&&endpoint.is_loopback())){return Err(Error::Denied);}
    Ok(())
}
pub fn source(vault: &Vault, id: &str) -> Result<(connections::Connection, Vec<String>)> {
    vault.expect_kind(id, &["ApprovedSourceRef"])?;
    let (connection, domains, enabled): (String,String,i64) = vault.db().query_row("SELECT connection_id,domains,enabled FROM network_sources WHERE id=?", [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or(Error::Denied)?;
    let connection = connections::load(vault, &connection)?;
    check_search_connection(&connection)?;
    if enabled!=1{return Err(Error::Denied);}
    Ok((connection, network::checked_domains(&serde_json::from_str::<Vec<String>>(&domains)?)?))
}

pub struct Binding {
    pub connection_id: String, pub name: String, pub descriptor: Value, pub schema_hash: String, pub server_hash: String,
    pub endpoint: Endpoint, pub credential: Option<String>, pub timeout_s: u64, pub proxy: ProxyMode,
}
pub fn binding(vault: &Vault, id: &str) -> Result<Binding> {
    vault.expect_kind(id, &["ApprovedToolBindingRef"])?;
    let row = vault.db().query_row("SELECT b.connection_id,b.name,b.descriptor,b.schema_hash,b.server_hash,c.endpoint,c.credential,c.timeout_s,c.proxy FROM external_bindings b JOIN external_connections c ON c.id=b.connection_id WHERE b.id=? AND b.active=1 AND c.enabled=1", [id], |r| {
        Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,Option<String>>(6)?,r.get::<_,i64>(7)?,r.get::<_,String>(8)?))
    }).optional()?.ok_or(Error::Denied)?;
    Ok(Binding {connection_id:row.0,name:row.1,descriptor:serde_json::from_str(&row.2)?,schema_hash:row.3,server_hash:row.4,endpoint:Endpoint::parse(&row.5)?,credential:row.6,timeout_s:row.7 as u64,proxy:if row.8=="none" {ProxyMode::None} else {ProxyMode::System}})
}

/// The equality check binds *all* fields, including purpose, to this root.
pub fn claim_action(ctx:&Ctx, invocation:&Invocation, input:&Value) -> Result<String> {
    ctx.check()?;
    let vault=ctx.lock()?;
    let row: Option<(String,String,String,String)> = vault.db().query_row("SELECT id,input,state,expires_at FROM network_actions WHERE root=? AND skill=?", params![invocation.root_id(),invocation.skill()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let (id, approved, state, expires)=row.ok_or(Error::Denied)?;
    if state!="approved" || serde_json::from_str::<Value>(&approved)? != *input || chrono::DateTime::parse_from_rfc3339(&expires).map_err(|_|Error::Integrity)? <= chrono::Utc::now() {return Err(Error::Denied);}
    if vault.db().execute("UPDATE network_actions SET state='running' WHERE id=? AND state='approved'", [&id])? != 1 {return Err(Error::Denied);}
    Ok(id)
}
pub fn finish_action(ctx:&Ctx, action:&str, result:&Result<EnvelopeParts>) -> Result<()> {
    let vault=ctx.lock()?;
    let (state, reference) = match result {Ok(parts)=>("done",parts.data_ref.clone()),Err(Error::Cancelled)=>("unknown",None),Err(_)=>("failed",None)};
    vault.db().execute("UPDATE network_actions SET state=?,result_ref=? WHERE id=? AND state='running'", params![state,reference,action])?;
    Ok(())
}
fn request(ctx:&Ctx, invocation:&Invocation, action:&str, host:&str, operation:&str) -> Result<String> {
    ctx.check()?;
    let vault=ctx.lock()?;
    let (state,expires):(String,String)=vault.db().query_row("SELECT state,expires_at FROM network_actions WHERE id=? AND root=?",params![action,invocation.root_id()],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if state!="running" || chrono::DateTime::parse_from_rfc3339(&expires).map_err(|_|Error::Integrity)?<=chrono::Utc::now() {return Err(Error::Denied);}
    let count:i64=vault.db().query_row("SELECT COUNT(*) FROM network_requests WHERE root=?",[invocation.root_id()],|r|r.get(0))?;
    if count>=6 {return Err(Error::Budget);}
    let id=format!("net_{}",uuid::Uuid::new_v4().simple());
    vault.db().execute("INSERT INTO network_requests(id,action_id,root,host,operation,status,started_at) VALUES(?,?,?,?,?,'sent',?)",params![id,action,invocation.root_id(),host,operation,now_rfc3339()])?;
    Ok(id)
}
fn finish_request(ctx:&Ctx, id:&str, result:&Result<Value>) -> Result<()> {
    let (status,error)=match result {Ok(_)=>("completed",None),Err(Error::Cancelled)=>("unknown",Some("cancelled".into())),Err(e)=>("failed",Some(e.code()))};
    ctx.lock()?.db().execute("UPDATE network_requests SET status=?,error=?,finished_at=? WHERE id=?",params![status,error,now_rfc3339(),id])?;
    Ok(())
}
fn short(value:&Value, key:&str, limit:usize)->Result<String>{
    let text=value[key].as_str().ok_or(Error::Contract)?;
    if text.chars().count()>limit {return Err(Error::Remote("response_too_large".into()));}
    Ok(text.into())
}

pub fn research(ctx:&Ctx, invocation:&Invocation, input:&Value)->Result<EnvelopeParts>{
    let query=text_of(input,"query")?;
    let fetch=query.strip_prefix("fetch:");
    ctx.authorize(invocation,if fetch.is_some(){"network.fetch"}else{"network.search"})?;
    let action=claim_action(ctx,invocation,input)?;
    let result=(||{
        let preview:String=ctx.lock()?.db().query_row("SELECT preview FROM network_actions WHERE id=?",[&action],|r|r.get(0))?;
        let preview:Value=serde_json::from_str(&preview)?;
        if preview["operation"]!=if fetch.is_some(){"fetch"}else{"search"}{return Err(Error::Denied);}
        let sources=refs_of(input,"source_ids")?;
        if sources.len()!=1 {return Err(Error::Contract);}
        let (connection,domains)={source(&*ctx.lock()?,&sources[0])?};
        let url=fetch.map(|url|network::public_url(url,&domains)).transpose()?;
        if url.is_none() && (query.trim().is_empty() || query.chars().count()>128){return Err(Error::NeedsInput("search_query_128"));}
        let key=connection.credential.as_ref().map(|id|credential_store(ctx)?.get(id)).transpose()?.ok_or(Error::NeedsInput("api_key"))?;
        let mut endpoint=connection.profile.endpoint.clone(); endpoint.base_path.clear();
        let operation=if url.is_some(){"/v1/tools/fetch"}else{"/v1/tools/search"};
        let domain=url.as_ref().and_then(|u|url::Url::parse(u).ok()).and_then(|u|u.host_str().map(str::to_owned));
        ctx.services.external_rate.wait(operation,domain.as_deref(),ctx.cancel)?;
        let body=if let Some(url)=&url{json!({"url":url})}else{json!({"query":query,"count":5,"need_content":false})};
        let audit=request(ctx,invocation,&action,&endpoint.display_host(),operation)?;
        // Recheck a revoked source immediately before handing off the request.
        source(&*ctx.lock()?,&sources[0])?;
        let reply=network::post(endpoint,operation.into(),body,Some(key),HttpOptions{timeout:Duration::from_secs(connection.profile.timeout_s.min(120)),proxy:connection.profile.proxy.clone()},vec![],&ctx.services.external_slots,ctx.cancel).map(|v|v.value);
        finish_request(ctx,&audit,&reply)?;
        let reply=reply?;
        ctx.check()?; source(&*ctx.lock()?,&sources[0])?;
        let accessed=now_rfc3339();
        let mut omitted=0;
        let items=if let Some(url)=url {
            let content=short(&reply,"content",network::MAX_TEXT)?;
            if content.trim().is_empty(){vec![]}else{vec![json!({"url":url,"title":short(&reply,"title",1000)?,"text":content,"sha256":sha256_hex(content.as_bytes()),"accessed_at":accessed,"published_at":null,"publisher":domain,"snapshot":"fetched-markdown"})]}
        }else{
            let rows=reply["results"].as_array().ok_or(Error::Remote("malformed_response".into()))?;
            if rows.len()>50{return Err(Error::Budget);}
            let mut items=vec![];
            for row in rows.iter().take(5){
                let candidate=match row["url"].as_str().and_then(|s|network::public_url(s,&domains).ok()){Some(url)=>url,None=>{omitted+=1;continue}};
                let snippet=short(row,"snippet",4000)?;
                let summary=short(row,"summary",8000)?;
                let text=format!("{snippet}\n{summary}");
                let published=row["date"].as_str().filter(|d|chrono::DateTime::parse_from_rfc3339(d).is_ok());
                let authority=row["authority"].as_i64().filter(|n|(-1..=4).contains(n));
                items.push(json!({"url":candidate,"title":short(row,"title",1000)?,"text":text,"sha256":sha256_hex(text.as_bytes()),"accessed_at":accessed,"published_at":published,
                    "publisher":row["site_name"].as_str().map(|s|s.chars().take(200).collect::<String>()),"service_authority":authority,"snapshot":"search-snippet-unverified"}));
            }
            items
        };
        let payload=json!({"query":query,"purpose":input["research_purpose"],"provider":"deepseek-tools","source_id":sources[0],"items":items,"omitted_sources":omitted,
            "accessed_at":accessed,"trust":"data_only","note":"搜索摘要与服务评分不证明法律效力；精读原文后仍须核对时点、地区和适用范围。","requests":1});
        let id=ctx.lock()?.put_result(invocation.skill(),invocation.id(),"WebResearchResultRef",&payload)?;
        let mut parts=completed(Some(id));
        if items.is_empty(){warn(&mut parts,"no_approved_results","没有取得白名单范围内的有效结果；未补发其他请求。");}
        Ok(parts)
    })();
    finish_action(ctx,&action,&result)?;result
}

pub fn tools_call(ctx:&Ctx,invocation:&Invocation,input:&Value)->Result<EnvelopeParts>{
    ctx.authorize(invocation,"connector.read")?;
    let action=claim_action(ctx,invocation,input)?;
    let result=(||{
        let binding_id=text_of(input,"binding_id")?;
        let args_id=text_of(input,"arguments_ref")?;
        let row=binding(&*ctx.lock()?,&binding_id)?;
        let args={let vault=ctx.lock()?;vault.expect_kind(&args_id,&["ValidatedTypedArgumentsRef"])?;
            let text:String=vault.db().query_row("SELECT value FROM external_arguments WHERE id=? AND binding_id=?",params![args_id,binding_id],|r|r.get(0)).optional()?.ok_or(Error::Denied)?;serde_json::from_str::<Value>(&text)?};
        mcp::validate_arguments(&row.descriptor["inputSchema"],&args)?;
        let key=row.credential.as_ref().map(|id|credential_store(ctx)?.get(id)).transpose()?;
        let catalog=row.name=="__catalog__";
        let call=(!catalog).then(||mcp::Call{name:row.name.clone(),descriptor_hash:row.schema_hash.clone(),server_hash:row.server_hash.clone(),arguments:args.clone()});
        let audit=request(ctx,invocation,&action,&row.endpoint.display_host(),if catalog{"mcp.discover"}else{"mcp.tools_call"})?;
        let guard_vault=ctx.vault.clone();let guard_action=action.clone();let guard_binding=binding_id.clone();
        let guard: mcp::RequestGuard=std::sync::Arc::new(move||{
            let vault=guard_vault.lock().map_err(|_|Error::Integrity)?;
            let (state,expires):(String,String)=vault.db().query_row("SELECT state,expires_at FROM network_actions WHERE id=?",[&guard_action],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if state!="running" || chrono::DateTime::parse_from_rfc3339(&expires).map_err(|_|Error::Integrity)?<=chrono::Utc::now(){return Err(Error::Denied);}
            binding(&vault,&guard_binding)?;Ok(())
        });
        let response=mcp::exchange(row.endpoint,key,HttpOptions{timeout:Duration::from_secs(row.timeout_s),proxy:row.proxy},call,&ctx.services.external_slots,ctx.cancel,Some(guard));
        finish_request(ctx,&audit,&response)?;
        if matches!(&response,Err(Error::Remote(code)) if code=="mcp_binding_changed") {ctx.lock()?.db().execute("UPDATE external_bindings SET active=0 WHERE id=?",[&binding_id])?;}
        let response=response?;
        ctx.check()?;binding(&*ctx.lock()?,&binding_id)?;
        let payload=json!({"connection_id":row.connection_id,"binding_id":binding_id,"tool":row.name,"schema_hash":row.schema_hash,"actual_sent":args,"exchange":response,"received_at":now_rfc3339(),"trust":"data_only"});
        let id=ctx.lock()?.put_result(invocation.skill(),invocation.id(),if catalog{"ToolCatalogRef"}else{"ExternalToolResultRef"},&payload)?;
        Ok(completed(Some(id)))
    })();
    finish_action(ctx,&action,&result)?;result
}

/// Protected connection-settings delegates its core-issued external drafts
/// here, retaining the same UI-only identity and credential permissions.
pub fn settings(ctx:&Ctx,invocation:&Invocation,draft_id:&str)->Result<Option<Value>>{
    let (profile,credential,consumed):(String,Option<String>,i64)=ctx.lock()?.db().query_row("SELECT profile,credential,consumed FROM connection_drafts WHERE id=?",[draft_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let profile:Value=serde_json::from_str(&profile)?;
    let Some(kind)=profile["external_kind"].as_str() else{return Ok(None)};
    if consumed!=0{return Err(Error::Conflict);}
    ctx.authorize(invocation,"credential.write")?;
    let value=&profile["value"];
    let credentials=credential_store(ctx)?;
    let vault=ctx.lock()?;
    let result=match kind{
        "source"=>{
            let connection=value["connection_id"].as_str().ok_or(Error::Contract)?;
            let connection=connections::load(&vault,connection)?;
            check_search_connection(&connection)?;
            let domains:Vec<String>=serde_json::from_value(value["domains"].clone())?;
            let domains=network::checked_domains(&domains)?;
            let id=vault.register("ApprovedSourceRef","m2-b",Some(&connection.id))?;
            vault.db().execute("INSERT INTO network_sources(id,connection_id,domains,enabled,created_at) VALUES(?,?,?,1,?)",params![id,connection.id,serde_json::to_string(&domains)?,now_rfc3339()])?;
            source(&vault,&id)?;json!({"source_id":id})
        }
        "mcp"=>{
            let name=value["name"].as_str().ok_or(Error::Contract)?.trim();
            if name.is_empty()||name.chars().count()>80{return Err(Error::Contract);}
            let endpoint=Endpoint::parse(value["endpoint"].as_str().ok_or(Error::Contract)?)?;
            url::Url::parse(&endpoint.base_url()).map_err(|_|Error::Contract)?;
            let timeout=value["timeout_s"].as_u64().unwrap_or(30);
            if !(5..=120).contains(&timeout){return Err(Error::Contract);}
            let proxy=match value["proxy"].as_str().unwrap_or("system"){"none"=>"none","system"=>"system",_=>return Err(Error::Contract)};
            let id=vault.register("ExternalConnectionRef","m2-b",None)?;
            vault.db().execute("INSERT INTO external_connections(id,name,endpoint,credential,timeout_s,proxy,enabled,created_at) VALUES(?,?,?,?,?,?,1,?)",params![id,name,endpoint.base_url(),credential,timeout as i64,proxy,now_rfc3339()])?;
            let descriptor=json!({"name":"__catalog__","inputSchema":{"type":"object","properties":{},"additionalProperties":false}});
            let binding=vault.register("ApprovedToolBindingRef","m2-b",Some(&id))?;
            vault.db().execute("INSERT INTO external_bindings(id,connection_id,name,descriptor,schema_hash,server_hash,created_at) VALUES(?,?,'__catalog__',?,?,'unbound',?)",params![binding,id,descriptor.to_string(),mcp::descriptor_hash(&descriptor),now_rfc3339()])?;
            json!({"connection_id":id})
        }
        "binding"=>{
            let catalog=value["catalog_ref"].as_str().ok_or(Error::Contract)?;
            vault.expect_kind(catalog,&["ToolCatalogRef"])?;
            let payload=vault.get_result(catalog)?.2;
            let received=payload["received_at"].as_str().ok_or(Error::Contract)?;
            if chrono::Utc::now()-chrono::DateTime::parse_from_rfc3339(received).map_err(|_|Error::Integrity)?.with_timezone(&chrono::Utc)>chrono::Duration::minutes(10){return Err(Error::Denied);}
            let name=value["tool_name"].as_str().ok_or(Error::Contract)?;
            let tools=payload["exchange"]["result"]["tools"].as_array().ok_or(Error::Contract)?;
            let tool=tools.iter().find(|t|t["name"]==name && t["annotations"]["readOnlyHint"]==true).ok_or(Error::Denied)?;
            mcp::validate_schema(&tool["inputSchema"])?;
            let connection=payload["connection_id"].as_str().ok_or(Error::Contract)?;
            let enabled:i64=vault.db().query_row("SELECT enabled FROM external_connections WHERE id=?",[connection],|r|r.get(0))?;
            if enabled!=1{return Err(Error::Denied);}
            let hash=mcp::descriptor_hash(tool);
            let server_hash=payload["exchange"]["server_hash"].as_str().ok_or(Error::Contract)?;
            let id=vault.register("ApprovedToolBindingRef","m2-b",Some(connection))?;
            vault.db().execute("INSERT INTO external_bindings(id,connection_id,name,descriptor,schema_hash,server_hash,created_at) VALUES(?,?,?,?,?,?,?)",params![id,connection,name,tool.to_string(),hash,server_hash,now_rfc3339()])?;
            json!({"binding_id":id,"schema_hash":hash})
        }
        "disable_source"=>{let id=value["id"].as_str().ok_or(Error::Contract)?;vault.expect_kind(id,&["ApprovedSourceRef"])?;vault.db().execute("UPDATE network_sources SET enabled=0 WHERE id=?",[id])?;json!({"disabled":id})}
        "disable_mcp"=>{let id=value["id"].as_str().ok_or(Error::Contract)?;vault.expect_kind(id,&["ExternalConnectionRef"])?;
            let credential:Option<String>=vault.db().query_row("SELECT credential FROM external_connections WHERE id=?",[id],|r|r.get(0))?;
            vault.db().execute("UPDATE external_connections SET enabled=0,credential=NULL WHERE id=?",[id])?;
            vault.db().execute("UPDATE external_bindings SET active=0 WHERE connection_id=?",[id])?;
            if let Some(id)=credential{credentials.remove(&id)?;}json!({"disabled":id})}
        "disable_package"=>{let id=value["id"].as_str().ok_or(Error::Contract)?;vault.expect_kind(id,&["KnowledgePackageRef"])?;vault.db().execute("UPDATE knowledge_packages SET state='disabled' WHERE id=?",[id])?;json!({"disabled":id})}
        _=>return Err(Error::Contract),
    };
    vault.db().execute("UPDATE connection_drafts SET consumed=1 WHERE id=?",[draft_id])?;
    Ok(Some(json!({"operation":kind,"result":result,"saved_at":now_rfc3339()})))
}
