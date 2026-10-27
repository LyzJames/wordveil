//! A user-owned document/question matrix, with a durable cell ledger. Only
//! native confirmation starts generation. Cells run sequentially and share
//! an aggregate request cap in addition to the original per-root limits.
use super::*;
use std::collections::BTreeSet;

pub const MAX_DOCUMENTS:usize=8;
pub const MAX_QUESTIONS:usize=4;
pub const CELL_REQUESTS:i64=7;

fn no_evidence(payload: &Value) -> bool {
    let steps = payload["process"].as_array().cloned().unwrap_or_default();
    payload["items"].as_array().is_some_and(Vec::is_empty)
        && steps.iter().any(|p| p["step"] == "search" && p["status"] == "completed")
        && !steps.iter().any(|p| matches!(p["step"].as_str(), Some("search" | "rerank")) && p["status"] != "completed")
}

fn check_cell(vault:&Vault,root:&str,current_reserved:bool)->Result<Option<(String,String)>> {
    let cell:Option<(String,String)>=vault.db().query_row("SELECT c.id,c.comparison_id FROM comparison_roots r JOIN comparison_cells c ON c.id=r.cell_id WHERE r.root=?",[root],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((cell,batch))=cell else{return Ok(None)};
    let approved:Option<String>=vault.db().query_row("SELECT approval_binding FROM comparisons WHERE id=?",[&batch],|r|r.get(0))?;
    if approved.as_deref()!=Some(binding(vault,&batch)?.as_str()){return Err(Error::Conflict);}
    let used:i64=vault.db().query_row("SELECT COALESCE(SUM(t.generation),0) FROM comparison_roots r JOIN roots t ON t.id=r.root WHERE r.cell_id=?",[&cell],|r|r.get(0))?;
    if used>CELL_REQUESTS || (!current_reserved&&used==CELL_REQUESTS){return Err(Error::Budget);}
    let active:bool=vault.db().query_row("SELECT EXISTS(SELECT 1 FROM comparisons b JOIN comparison_cells c ON c.comparison_id=b.id WHERE b.id=? AND c.id=? AND c.root=? AND b.state='running' AND c.state='running' AND b.approved_at IS NOT NULL AND b.used_requests<b.max_requests)",params![batch,cell,root],|r|r.get(0))?;
    if !active{return Err(Error::Budget);}
    Ok(Some((cell,batch)))
}
pub(crate) fn check(vault:&Vault,root:&str)->Result<()> {check_cell(vault,root,false).map(|_|())}

/// Called by the common generation boundary, after root reservation and
/// before any network byte. Retried roots remain linked to the same cell.
pub(crate) fn reserve(vault:&Vault,root:&str)->Result<()> {
    let Some((cell,batch))=check_cell(vault,root,true)? else{return Ok(())};
    let used:i64=vault.db().query_row("SELECT COUNT(*) FROM generation_requests g JOIN comparison_roots r ON r.root=g.root WHERE r.cell_id=?",[&cell],|r|r.get(0))?;
    if used>=CELL_REQUESTS{return Err(Error::Budget);}
    let changed=vault.db().execute("UPDATE comparisons SET used_requests=used_requests+1,updated_at=? WHERE id=? AND state='running' AND approved_at IS NOT NULL AND used_requests<max_requests AND EXISTS(SELECT 1 FROM comparison_cells c WHERE c.id=? AND c.state='running' AND c.root=?)",params![now_rfc3339(),batch,cell,root])?;
    if changed!=1{return Err(Error::Budget);}
    Ok(())
}

/// An app exit/lock never silently starts the next cell on unlock. A result
/// committed just before exit is recovered from the trusted result store.
pub(crate) fn recover(vault:&Vault)->Result<()> {
    let rows:Vec<(String,Option<String>)>={let mut s=vault.db().prepare("SELECT id,root FROM comparison_cells WHERE state IN ('preparing','queued','running')")?;
        s.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<std::result::Result<_,_>>()?};
    for (cell,root) in rows {
        let result:Option<String>=match root.as_deref(){Some(root)=>vault.db().query_row("SELECT r.id FROM results r JOIN invocations i ON i.id=r.invocation JOIN objects o ON o.id=r.id WHERE i.root=? AND r.kind='RetrievalResultRef' AND o.active=1 ORDER BY r.rowid DESC LIMIT 1",[root],|r|r.get(0)).optional()?,None=>None};
        if let Some(id)=result {
            let p=vault.get_result(&id)?.2;
            let state=if !p["answer"].is_null(){"done"}else if no_evidence(&p){"no_evidence"}else{"failed"};
            vault.db().execute("UPDATE comparison_cells SET state=?,result_ref=?,reason='recovered_after_exit',updated_at=? WHERE id=?",params![state,id,now_rfc3339(),cell])?;
        }else{
            vault.db().execute("UPDATE comparison_cells SET state=?,reason='interrupted',updated_at=? WHERE id=?",params![if root.is_some(){"interrupted"}else{"pending"},now_rfc3339(),cell])?;
        }
    }
    vault.db().execute("UPDATE comparisons SET state='paused',updated_at=? WHERE state IN ('preparing','running')",[now_rfc3339()])?;
    Ok(())
}

fn binding(vault:&Vault,id:&str)->Result<String> {
    let (spec,connection,maximum):(String,Option<String>,i64)=vault.db().query_row("SELECT spec,connection_id,max_requests FROM comparisons WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let connection=connection.ok_or(Error::NeedsInput("connection"))?;
    let loaded=crate::generation::connections::load(vault,&connection)?;
    if loaded.status!="active"{return Err(Error::Denied);}
    let spec:Value=serde_json::from_str(&spec)?;
    if spec["connection_fingerprint"]!=json!(profile_fingerprint(&loaded)){return Err(Error::NeedsInput("comparison_changed"));}
    for row in spec["documents"].as_array().ok_or(Error::Integrity)? {
        let version=row["version_id"].as_str().ok_or(Error::Integrity)?;
        vault.expect_kind(version,&["DocumentVersionRef"])?;
        let (hash,chunks):(String,i64)=vault.db().query_row("SELECT content_sha256,(SELECT COUNT(*) FROM chunks c JOIN objects o ON o.id=c.id WHERE c.version_id=v.id AND o.active=1) FROM document_versions v WHERE v.id=?",[version],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let revision=chunk_revision(vault,version)?;
        if row["content_sha256"]!=json!(hash)||row["chunks"]!=json!(chunks)||row["chunk_revision"]!=json!(revision)||chunks==0{return Err(Error::Conflict);}
    }
    let mut stmt=vault.db().prepare("SELECT c.row_no,c.col_no,c.version_id,c.question_id,s.matters,s.versions FROM comparison_cells c JOIN scopes s ON s.id=c.scope_id WHERE c.comparison_id=? ORDER BY c.row_no,c.col_no")?;
    let mut cells=stmt.query([id])?;let mut count=0;
    while let Some(cell)=cells.next()? {
        count+=1;let row=cell.get::<_,i64>(0)? as usize;let col=cell.get::<_,i64>(1)? as usize;
        let version:String=cell.get(2)?;let question:String=cell.get(3)?;
        let matters:Value=serde_json::from_str(&cell.get::<_,String>(4)?)?;let versions:Value=serde_json::from_str(&cell.get::<_,String>(5)?)?;
        if spec["documents"][row]["version_id"]!=json!(version)||spec["question_refs"][col]!=json!(question)||versions!=json!([version])||matters!=json!([spec["documents"][row]["matter_id"]]){return Err(Error::Integrity);}
    }
    if count!=spec["documents"].as_array().ok_or(Error::Integrity)?.len()*spec["questions"].as_array().ok_or(Error::Integrity)?.len(){return Err(Error::Integrity);}
    Ok(sha256_hex(&serde_json::to_vec(&json!({"spec":spec,"connection_id":connection,"profile_binding":loaded.profile.binding(),"max_output_tokens":loaded.profile.max_output_tokens,"timeout_s":loaded.profile.timeout_s,"proxy":format!("{:?}",loaded.profile.proxy),"maximum":maximum,"version":crate::VERSION}))?))
}
fn profile_fingerprint(connection:&crate::generation::connections::Connection)->String {
    sha256_hex(format!("{}|{}|{}|{}|{:?}",connection.profile.binding(),connection.profile.provider.as_str(),connection.profile.max_output_tokens,connection.profile.timeout_s,connection.profile.proxy).as_bytes())
}
fn chunk_revision(vault:&Vault,version:&str)->Result<String> {
    use sha2::Digest;
    let mut digest=sha2::Sha256::new();let mut stmt=vault.db().prepare("SELECT c.id FROM chunks c JOIN objects o ON o.id=c.id WHERE c.version_id=? AND o.active=1 ORDER BY c.ordinal,c.id")?;
    for row in stmt.query_map([version],|r|r.get::<_,String>(0))? {digest.update(row?.as_bytes());digest.update(b"\n");}
    Ok(format!("{:x}",digest.finalize()))
}
impl CoreService {
    fn comparison_owner(&self,id:&str)->Result<VaultHandle> {
        let handle=self.vault()?;
        self.lock_vault(&handle)?.expect_kind(id,&["ComparisonRef"])?;
        Ok(handle)
    }
    fn comparison_binding(&self,vault:&Vault,id:&str)->Result<String> {
        binding(vault,id)
    }
    pub fn comparison_create(&self,title:&str,versions:&[String],questions:&[String],connection:Option<&str>,max_requests:i64)->Result<String> {
        if title.trim().is_empty()||title.chars().count()>120||versions.is_empty()||versions.len()>MAX_DOCUMENTS||questions.is_empty()||questions.len()>MAX_QUESTIONS||versions.iter().collect::<BTreeSet<_>>().len()!=versions.len()||max_requests<1||max_requests>versions.len() as i64*questions.len() as i64*CELL_REQUESTS{return Err(Error::Contract);}
        let connection=self.connection_for(connection)?;
        let handle=self.vault()?;let cancel=self.session_cancel(&handle)?;let sink=|_:Progress|{};
        let connection_fingerprint={let vault=self.lock_vault(&handle)?;profile_fingerprint(&crate::generation::connections::load(&vault,&connection)?)};
        let ctx=Ctx{vault:&handle,services:&self.services,cancel:&cancel,progress:&sink};
        handlers::indexing::ensure_dense(&ctx)?;
        let mut documents=vec![];let mut scopes=vec![];
        for version in versions {
            let (matter,name,number,hash,chunks)={let vault=self.lock_vault(&handle)?;
                vault.expect_kind(version,&["DocumentVersionRef"])?;
                vault.db().query_row("SELECT d.matter_id,d.display_name,v.version_no,v.content_sha256,(SELECT COUNT(*) FROM chunks c JOIN objects o ON o.id=c.id WHERE c.version_id=v.id AND o.active=1) FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?",[version],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?)))?};
            if chunks<=0{return Err(Error::NeedsInput("index_not_ready"));}
            scopes.push(self.create_scope(&[matter.clone()],&[version.clone()])?);
            let revision={let vault=self.lock_vault(&handle)?;chunk_revision(&vault,version)?};
            documents.push(json!({"version_id":version,"matter_id":matter,"display_name":name,"version_no":number,"content_sha256":hash,"chunks":chunks,"chunk_revision":revision}));
        }
        let mut question_refs=vec![];
        for question in questions {question_refs.push(self.create_question(question)?);}
        let vault=self.lock_vault(&handle)?;let now=now_rfc3339();
        let id=vault.register("ComparisonRef", "0.4.0",None)?;
        let spec=json!({"documents":documents,"questions":questions,"question_refs":question_refs,"per_cell_max_requests":CELL_REQUESTS,"order":"row_then_column","space":self.services.layout.space.id,"connection_fingerprint":connection_fingerprint});
        vault.db().execute("INSERT INTO comparisons(id,title,spec,connection_id,state,max_requests,created_at,updated_at) VALUES(?,?,?,?,'draft',?,?,?)",params![id,title.trim(),spec.to_string(),connection,max_requests,now,now])?;
        for (row,version) in versions.iter().enumerate() {for (col,question) in question_refs.iter().enumerate(){
            let cell=vault.register("ComparisonCellRef", "0.4.0",Some(&id))?;
            vault.db().execute("INSERT INTO comparison_cells(id,comparison_id,row_no,col_no,version_id,question_id,scope_id,state,updated_at) VALUES(?,?,?,?,?,?,?,'pending',?)",params![cell,id,row as i64,col as i64,version,question,scopes[row],now])?;
        }}
        Ok(id)
    }
    pub fn comparison_list(&self)->Result<Vec<Value>> {
        let handle=self.vault()?;let vault=self.lock_vault(&handle)?;
        let mut s=vault.db().prepare("SELECT id,title,state,max_requests,used_requests,created_at,updated_at FROM comparisons ORDER BY updated_at DESC LIMIT 100")?;
        Ok(s.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"title":r.get::<_,String>(1)?,"state":r.get::<_,String>(2)?,"max_requests":r.get::<_,i64>(3)?,"used_requests":r.get::<_,i64>(4)?,"created_at":r.get::<_,String>(5)?,"updated_at":r.get::<_,String>(6)?})))?.collect::<std::result::Result<_,_>>()?)
    }
    pub fn comparison_read(&self,id:&str)->Result<Value> {
        let handle=self.comparison_owner(id)?;let vault=self.lock_vault(&handle)?;
        let (title,spec,state,maximum,used):(String,String,String,i64,i64)=vault.db().query_row("SELECT title,spec,state,max_requests,used_requests FROM comparisons WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let mut s=vault.db().prepare("SELECT c.id,c.row_no,c.col_no,c.state,c.reason,c.result_ref,c.task_id,c.run_id,c.attempts,(SELECT COUNT(*) FROM generation_requests g JOIN comparison_roots r ON r.root=g.root WHERE r.cell_id=c.id),(SELECT COALESCE(SUM(t.generation),0) FROM comparison_roots r JOIN roots t ON t.id=r.root WHERE r.cell_id=c.id) FROM comparison_cells c WHERE c.comparison_id=? ORDER BY c.row_no,c.col_no")?;
        let mut cells=s.query_map([id],|r|{let attempts=r.get::<_,i64>(10)?;Ok(json!({"id":r.get::<_,String>(0)?,"row":r.get::<_,i64>(1)?,"col":r.get::<_,i64>(2)?,"state":r.get::<_,String>(3)?,"reason":r.get::<_,Option<String>>(4)?,"result_ref":r.get::<_,Option<String>>(5)?,"task_id":r.get::<_,Option<String>>(6)?,"run_id":r.get::<_,Option<String>>(7)?,"attempts":r.get::<_,i64>(8)?,"used_requests":r.get::<_,i64>(9)?,"used_attempts":attempts,"remaining_requests":CELL_REQUESTS.saturating_sub(attempts)}))})?.collect::<std::result::Result<Vec<_>,_>>()?;
        drop(s);drop(vault);
        let tasks=self.tasks()?;
        for cell in &mut cells {
            if let Some(code) = cell["reason"].as_str().filter(|s| !s.is_empty()) {
                cell["reason_message"] = json!(crate::handlers::error_message(code).unwrap_or_else(|| match code {
                    "interrupted" => "任务已中断；恢复时沿用现有计数。".into(),
                    "recovered_after_exit" => "已从上次退出前保留的结果恢复。".into(),
                    _ => format!("处理未完成（{}）；请查看原文和任务处理过程。", crate::sanitize_code(&code.replace(':', "_"))),
                }));
            }
            if let Some(task)=tasks.iter().find(|t|cell["task_id"]==json!(t.task_id)){cell["progress"]=serde_json::to_value(&task.progress)?;if task.status=="running" && matches!(cell["state"].as_str(),Some("interrupted"|"failed")){cell["state"]=json!("stopping");}}
        }
        Ok(json!({"id":id,"title":title,"spec":serde_json::from_str::<Value>(&spec)?,"state":state,"max_requests":maximum,"used_requests":used,"remaining_requests":maximum.saturating_sub(used),"cells":cells}))
    }
    /// Prepare previews locally. The DB records which cell is being prepared,
    /// so navigation, polling and interruption never hide the batch progress.
    pub fn comparison_prepare(self:&Arc<Self>,id:&str)->Result<()> {
        let handle=self.comparison_owner(id)?;
        let binding={let vault=self.lock_vault(&handle)?;
            let binding=self.comparison_binding(&vault,id)?;
            let changed=vault.db().execute("UPDATE comparisons SET state='preparing',approved_at=NULL,approval_binding=?,updated_at=? WHERE id=? AND state IN ('draft','paused','prepared','partial')",params![binding,now_rfc3339(),id])?;
            if changed!=1{return Err(Error::Conflict);}
            binding
        };
        let service=self.clone();let id=id.to_owned();
        std::thread::spawn(move||{let result=service.prepare_cells(&id,&handle,&binding);if let Err(error)=result {
            if let Ok(vault)=service.lock_vault(&handle){let _=vault.db().execute("UPDATE comparisons SET state='paused',updated_at=? WHERE id=? AND state='preparing'",params![now_rfc3339(),id]);let _=vault.db().execute("UPDATE comparison_cells SET state='failed',reason=?,updated_at=? WHERE comparison_id=? AND state='preparing'",params![error.code(),now_rfc3339(),id]);}
        }});
        Ok(())
    }
    fn prepare_cells(self:&Arc<Self>,id:&str,handle:&VaultHandle,expected_binding:&str)->Result<()> {
        let cells:Vec<(String,String,String,Option<String>)>={let vault=self.lock_vault(handle)?;let mut s=vault.db().prepare("SELECT id,question_id,scope_id,grant_id FROM comparison_cells WHERE comparison_id=? AND root IS NULL AND state NOT IN ('done','no_evidence','stale') AND (state<>'prepared' OR grant_id IS NULL) ORDER BY row_no,col_no")?;
            s.query_map([id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<std::result::Result<_,_>>()?};
        for (cell,question,scope,old) in cells {
            let (text,connection,input)={let vault=self.lock_vault(handle)?;
                let state:String=vault.db().query_row("SELECT state FROM comparisons WHERE id=?",[id],|r|r.get(0))?;if state!="preparing"{return Err(Error::Cancelled);}
                if self.comparison_binding(&vault,id)?!=expected_binding{return Err(Error::Conflict);}
                vault.db().execute("UPDATE comparison_cells SET state='preparing',reason=NULL,updated_at=? WHERE id=?",params![now_rfc3339(),cell])?;
                if let Some(old)=old {vault.db().execute("UPDATE egress_grants SET revoked_at=? WHERE id=? AND approved_at IS NULL",params![now_rfc3339(),old])?;}
                let text:String=vault.db().query_row("SELECT text FROM questions WHERE id=?",[&question],|r|r.get(0))?;
                let connection:String=vault.db().query_row("SELECT connection_id FROM comparisons WHERE id=?",[id],|r|r.get(0))?;
                (text,connection,json!({"question_id":question,"scope_id":scope,"known_constraint_ids":[],"requested_output":"answer"}))};
            let local_input=json!({"question_id":question,"scope_id":scope,"known_constraint_ids":[],"requested_output":"evidence_only"});
            let task=self.start_skill("lvjin-retrieval",local_input,"ui")?;
            {let vault=self.lock_vault(handle)?;vault.db().execute("UPDATE comparison_cells SET task_id=? WHERE id=?",params![task,cell])?;}
            let items=loop {
                std::thread::sleep(Duration::from_millis(100));
                let vault=self.lock_vault(handle)?;let state:String=vault.db().query_row("SELECT state FROM comparisons WHERE id=?",[id],|r|r.get(0))?;drop(vault);
                if state!="preparing"{let _=self.interrupt_task(&task);return Err(Error::Cancelled);}
                let task=self.task(&task)?;if task.status=="running"{continue;}
                let envelope=task.envelope.ok_or(Error::Integrity)?;
                let result=envelope["data_ref"].as_str().ok_or(Error::Worker("comparison_prepare_failed".into()))?;
                break self.result(result)?["payload"]["items"].as_array().cloned().unwrap_or_default();
            };
            let preview=self.grant_preview("answer",&text,&question,&scope,1,Some(&connection),input,Excerpts::Ready(items),json!({"comparison_id":id,"cell_id":cell}))?;
            let vault=self.lock_vault(handle)?;
            let state:String=vault.db().query_row("SELECT state FROM comparisons WHERE id=?",[id],|r|r.get(0))?;
            if state!="preparing"{self.answer_cancel_preview_after_unlock(&vault,preview["grant_id"].as_str().ok_or(Error::Integrity)?)?;return Err(Error::Cancelled);}
            vault.db().execute("UPDATE comparison_cells SET grant_id=?,state='prepared',updated_at=? WHERE id=? AND root IS NULL",params![preview["grant_id"].as_str(),now_rfc3339(),cell])?;
        }
        let vault=self.lock_vault(handle)?;
        let binding=self.comparison_binding(&vault,id)?;
        if binding!=expected_binding{return Err(Error::Conflict);}
        vault.db().execute("UPDATE comparisons SET state='prepared',approval_binding=?,updated_at=? WHERE id=? AND state='preparing'",params![binding,now_rfc3339(),id])?;
        Ok(())
    }
    fn answer_cancel_preview_after_unlock(&self,vault:&Vault,grant:&str)->Result<()> {
        vault.db().execute("UPDATE egress_grants SET revoked_at=? WHERE id=? AND approved_at IS NULL",params![now_rfc3339(),grant])?;Ok(())
    }
    /// Stored parameters for both the in-app preview and the native dialog.
    /// `selected` is empty for the unfinished batch, or one/more failed cells.
    pub fn comparison_preview(&self,id:&str,selected:&[String])->Result<Value> {
        let handle=self.comparison_owner(id)?;let vault=self.lock_vault(&handle)?;
        let (state,stored,approved,maximum,used,connection,spec,title):(String,Option<String>,Option<String>,i64,i64,String,String,String)=vault.db().query_row("SELECT state,approval_binding,approved_at,max_requests,used_requests,connection_id,spec,title FROM comparisons WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?;
        if !matches!(state.as_str(),"prepared"|"paused"|"partial"|"completed") {return Err(Error::Conflict);}
        let binding=self.comparison_binding(&vault,id)?;
        if stored.as_deref()!=Some(binding.as_str()){return Err(Error::NeedsInput("comparison_changed"));}
        if used>=maximum{return Err(Error::Budget);}
        let loaded=crate::generation::connections::load(&vault,&connection)?;
        let mut s=vault.db().prepare("SELECT id,state,grant_id,row_no,col_no,root FROM comparison_cells WHERE comparison_id=? ORDER BY row_no,col_no")?;
        let rows=s.query_map([id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,Option<String>>(5)?)))?;
        let mut cells=vec![];let mut chosen=BTreeSet::new();
        for row in rows {let (cell,state,grant,row,col,root)=row?;
            if matches!(state.as_str(),"done"|"no_evidence"|"stale"){if selected.contains(&cell){return Err(Error::Denied);}continue;}
            if !selected.is_empty()&&!selected.contains(&cell){continue;}
            let task:Option<String>=vault.db().query_row("SELECT task_id FROM comparison_cells WHERE id=?",[&cell],|r|r.get(0))?;
            if task.as_deref().is_some_and(|id|self.task(id).is_ok_and(|t|t.status=="running")){return Err(Error::NeedsInput("comparison_wait_stop"));}
            if !matches!(state.as_str(),"prepared"|"failed"|"interrupted"|"budget"|"pending"){return Err(Error::NeedsInput("comparison_prepare"));}
            let grant=grant.ok_or(Error::NeedsInput("comparison_prepare"))?;
            let preview:String=vault.db().query_row("SELECT preview FROM egress_grants WHERE id=? AND revoked_at IS NULL",[&grant],|r|r.get(0)).optional()?.ok_or(Error::Denied)?;
            let mut preview:Value=serde_json::from_str(&preview)?;
            preview["grant_id"]=json!(grant);preview["question_id"]=preview["skill_input"]["question_id"].clone();preview["scope_id"]=preview["skill_input"]["scope_id"].clone();
            let cell_used:i64=vault.db().query_row("SELECT COALESCE(SUM(t.generation),0) FROM comparison_roots r JOIN roots t ON t.id=r.root WHERE r.cell_id=?",[&cell],|r|r.get(0))?;
            if cell_used>=CELL_REQUESTS {if !selected.is_empty(){return Err(Error::Budget);}continue;}
            let unknown:i64=vault.db().query_row("SELECT COUNT(*) FROM generation_requests g JOIN comparison_roots r ON r.root=g.root WHERE r.cell_id=? AND (g.status='unknown' OR (g.status='failed' AND g.error='cancelled'))",[&cell],|r|r.get(0))?;
            chosen.insert(cell.clone());cells.push(json!({"id":cell,"row":row,"col":col,"state":state,"used_requests":cell_used,"remaining_requests":CELL_REQUESTS-cell_used,"unknown_requests":unknown,"preview":preview,"continuation":root.is_some()}));
        }
        if cells.is_empty()||selected.iter().any(|s|!chosen.contains(s)){return Err(Error::NeedsInput("comparison_cells"));}
        Ok(json!({"id":id,"title":title,"binding":binding,"spec":serde_json::from_str::<Value>(&spec)?,"connection":{"id":connection,"name":loaded.name,"host":loaded.profile.endpoint.display_host(),"model_id":loaded.profile.model_id},"max_requests":maximum,"used_requests":used,"remaining_requests":maximum-used,"per_cell_max_requests":CELL_REQUESTS,"max_output_tokens":loaded.profile.max_output_tokens,"cells":cells,"previously_approved":approved.is_some(),"validity":"this_batch_and_selected_cells"}))
    }
    /// Only the shell's real native confirmation calls this in production.
    pub fn comparison_start(self:&Arc<Self>,id:&str,selected:&[String],binding:&str)->Result<()> {
        let preview=self.comparison_preview(id,selected)?;
        if preview["binding"]!=json!(binding){return Err(Error::Conflict);}
        let cells:Vec<String>=preview["cells"].as_array().ok_or(Error::Integrity)?.iter().filter_map(|c|c["id"].as_str().map(str::to_owned)).collect();
        let handle=self.comparison_owner(id)?;
        {let mut vault=self.lock_vault(&handle)?;
            let active:bool=vault.db().query_row("SELECT EXISTS(SELECT 1 FROM comparisons WHERE state IN ('running','preparing'))",[],|r|r.get(0))?;
            if active{return Err(Error::Conflict);}
            if self.comparison_binding(&vault,id)?!=binding{return Err(Error::Conflict);}
            let tx=vault.runtime.ledger.db.transaction()?;
            tx.execute("UPDATE comparisons SET state='running',approved_at=?,updated_at=? WHERE id=?",params![now_rfc3339(),now_rfc3339(),id])?;
            for cell in &cells {tx.execute("UPDATE comparison_cells SET state='queued',reason=NULL,updated_at=? WHERE id=? AND comparison_id=?",params![now_rfc3339(),cell,id])?;}
            tx.commit()?;
        }
        let service=self.clone();let id=id.to_owned();let binding=binding.to_owned();
        std::thread::spawn(move||{
            let result=service.run_comparison(&id,&cells,&binding,&handle);
            if let Ok(vault)=service.lock_vault(&handle){
                let unfinished:bool=vault.db().query_row("SELECT EXISTS(SELECT 1 FROM comparison_cells WHERE comparison_id=? AND state NOT IN ('done','no_evidence','stale'))",[&id],|r|r.get(0)).unwrap_or(true);
                let state=if result.is_err(){"paused"}else if unfinished{"partial"}else{"completed"};
                let _=vault.db().execute("UPDATE comparisons SET state=?,updated_at=? WHERE id=? AND state='running'",params![state,now_rfc3339(),id]);
                let _=vault.db().execute("UPDATE comparison_cells SET state='interrupted',reason='batch_stopped',updated_at=? WHERE comparison_id=? AND state IN ('queued','running')",params![now_rfc3339(),id]);
            }
        });Ok(())
    }
    fn run_comparison(self:&Arc<Self>,id:&str,cells:&[String],binding:&str,handle:&VaultHandle)->Result<()> {
        for cell in cells {
            let (grant,root,run)={let vault=self.lock_vault(handle)?;
                let (state,used,maximum):(String,i64,i64)=vault.db().query_row("SELECT state,used_requests,max_requests FROM comparisons WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                if state!="running"{return Err(Error::Cancelled);}if used>=maximum{return Err(Error::Budget);}
                if self.comparison_binding(&vault,id)?!=binding{return Err(Error::Conflict);}
                vault.db().execute("UPDATE comparison_cells SET state='running',attempts=attempts+1,updated_at=? WHERE id=? AND state='queued'",params![now_rfc3339(),cell])?;
                vault.db().query_row("SELECT grant_id,root,(SELECT f.id FROM flow_runs f WHERE f.root=c.root AND f.status='interrupted' ORDER BY f.updated_at DESC LIMIT 1) FROM comparison_cells c WHERE c.id=?",[cell],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?)))?};
            let run=match run {Some(run)=>{let stale=self.describe_continue(&run)?["stale"]==json!(true);if stale{self.lock_vault(handle)?.db().execute("UPDATE flow_runs SET status='stale',reason='comparison_explicit_reretrieval',updated_at=? WHERE id=?",params![now_rfc3339(),run])?;None}else{Some(run)}},None=>None};
            let task=if let Some(run)=run {self.flow_continue(&run)}else if let Some(root)=root {
                let resumed={let vault=self.lock_vault(handle)?;let owner:Option<String>=vault.db().query_row("SELECT id FROM invocations WHERE root=? AND depth=1 ORDER BY rowid DESC LIMIT 1",[&root],|r|r.get(0)).optional()?;
                    match owner {Some(owner)=>vault.runtime.reload_invocation(&owner).ok(),None=>None}};
                if let Some(invocation)=resumed {
                    let input:Value={let vault=self.lock_vault(handle)?;let preview:String=vault.db().query_row("SELECT preview FROM egress_grants WHERE id=? AND root=? AND revoked_at IS NULL",params![grant,root],|r|r.get(0))?;serde_json::from_str::<Value>(&preview)?["skill_input"].clone()};
                    self.spawn("lvjin-retrieval",input,handle.clone(),invocation)
                }else {
                    // The user explicitly selected this failed cell again.
                    // A cancelled root stays cancelled; a linked local
                    // continuation gets only the cell's remaining allowance.
                    let next={let vault=self.lock_vault(handle)?;
                        let used:i64=vault.db().query_row("SELECT COALESCE(SUM(t.generation),0) FROM comparison_roots r JOIN roots t ON t.id=r.root WHERE r.cell_id=?",[cell],|r|r.get(0))?;
                        if used>=CELL_REQUESTS{return Err(Error::Budget);}
                        let (connection,host,purpose,scope,question,items,preview):(String,String,String,String,String,i64,String)=vault.db().query_row("SELECT connection_id,host,purpose,scope_id,question_id,max_items,preview FROM egress_grants WHERE id=?",[&grant],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
                        let mut preview:Value=serde_json::from_str(&preview)?;preview["continuation_of"]=json!(root);preview["max_requests"]=json!(CELL_REQUESTS-used);
                        let next=vault.register("EgressGrantRef", "0.4.0-cell-continuation",Some(&question))?;
                        vault.db().execute("INSERT INTO egress_grants(id,connection_id,host,purpose,scope_id,question_id,max_items,max_requests,preview,created_at) VALUES(?,?,?,?,?,?,?,?,?,?)",params![next,connection,host,purpose,scope,question,items,CELL_REQUESTS-used,preview.to_string(),now_rfc3339()])?;
                        vault.db().execute("UPDATE comparison_cells SET grant_id=? WHERE id=?",params![next,cell])?;next
                    };
                    self.grant_start(&next,&[],&[])
                }
            }else {self.grant_start(&grant,&[],&[])};
            let task=match task {Ok(task)=>task,Err(error)=>{let vault=self.lock_vault(handle)?;vault.db().execute("UPDATE comparison_cells SET state='failed',reason=?,updated_at=? WHERE id=?",params![error.code(),now_rfc3339(),cell])?;continue;}};
            {let vault=self.lock_vault(handle)?;vault.db().execute("UPDATE comparison_cells SET task_id=?,updated_at=? WHERE id=?",params![task,now_rfc3339(),cell])?;}
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let current=self.vault()?;if !Arc::ptr_eq(&current,handle){return Err(Error::Cancelled);}
                let state=self.task(&task)?;
                if state.status=="running" {
                    let vault=self.lock_vault(handle)?;let status:String=vault.db().query_row("SELECT state FROM comparisons WHERE id=?",[id],|r|r.get(0))?;drop(vault);
                    if status!="running"{let _=self.interrupt_task(&task);return Err(Error::Cancelled);}continue;
                }
                let envelope=state.envelope.unwrap_or(Value::Null);
                let result=envelope["data_ref"].as_str();
                let vault=self.lock_vault(handle)?;
                let payload=match result {Some(result)=>vault.get_result(result).ok().map(|r|r.2),None=>None};
                let budget=envelope["warnings"].as_array().is_some_and(|ws|ws.iter().any(|w|w["code"]=="budget"));
                let cell_state=if state.status=="interrupted"{"interrupted"}else if payload.as_ref().is_some_and(|p|!p["answer"].is_null()){"done"}else if payload.as_ref().is_some_and(no_evidence){"no_evidence"}else if budget{"budget"}else{"failed"};
                let warning_codes: Vec<&str> = envelope["warnings"].as_array().map(|ws| ws.iter().filter_map(|w| w["code"].as_str()).collect()).unwrap_or_default();
                let reason=warning_codes.iter().copied().find(|code| crate::handlers::error_message(code).is_some()).or_else(|| warning_codes.last().copied()).unwrap_or("");
                vault.db().execute("UPDATE comparison_cells SET state=?,reason=?,run_id=?,result_ref=?,updated_at=? WHERE id=?",params![cell_state,reason,envelope["flow_run"].as_str(),result,now_rfc3339(),cell])?;
                // Unknown outcomes need a new explicit cell continuation;
                // subsequent independent cells can still proceed in budget.
                break;
            }
        }
        Ok(())
    }
    pub fn comparison_pause(&self,id:&str)->Result<()> {
        let handle=self.comparison_owner(id)?;
        let tasks:Vec<String>={let vault=self.lock_vault(&handle)?;
            vault.db().execute("UPDATE comparisons SET state='paused',updated_at=? WHERE id=? AND state IN ('running','preparing')",params![now_rfc3339(),id])?;
            let mut s=vault.db().prepare("SELECT task_id FROM comparison_cells WHERE comparison_id=? AND state IN ('running','preparing') AND task_id IS NOT NULL")?;
            s.query_map([id],|r|r.get(0))?.collect::<std::result::Result<_,_>>()?};
        for task in tasks {let _=self.interrupt_task(&task);}Ok(())
    }
    /// Changing a budget withdraws approval; the next start still requires
    /// a full preview and native confirmation. No counter is reset.
    pub fn comparison_budget(&self,id:&str,maximum:i64)->Result<()> {
        let handle=self.comparison_owner(id)?;let vault=self.lock_vault(&handle)?;
        let (state,used,count):(String,i64,i64)=vault.db().query_row("SELECT state,used_requests,(SELECT COUNT(*) FROM comparison_cells c WHERE c.comparison_id=b.id) FROM comparisons b WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        if matches!(state.as_str(),"running"|"preparing"|"stale"|"completed")||maximum<=used||maximum>count*CELL_REQUESTS{return Err(Error::Contract);}
        self.comparison_binding(&vault,id)?;
        vault.db().execute("UPDATE comparisons SET max_requests=?,approved_at=NULL,state=CASE WHEN state='draft' THEN 'draft' ELSE 'paused' END,updated_at=? WHERE id=?",params![maximum,now_rfc3339(),id])?;
        let binding=self.comparison_binding(&vault,id)?;
        vault.db().execute("UPDATE comparisons SET approval_binding=? WHERE id=?",params![binding,id])?;Ok(())
    }
}
