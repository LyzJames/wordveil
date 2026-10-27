use super::*;

fn nonempty(text:&str,max:usize)->Result<()> {if text.trim().is_empty()||text.len()>max||text.contains('\0'){Err(Error::Contract)}else{Ok(())}}
fn copy_rows(from:&Vault,to:&Vault,table:&str,filter:&str)->Result<()> {
    // Table/filter are hard-coded by temporary_start, never IPC input.
    let mut stmt=from.db().prepare(&format!("SELECT * FROM {table} {filter}"))?;let count=stmt.column_count();
    let rows=stmt.query_map([],|r|(0..count).map(|i|r.get::<_,rusqlite::types::Value>(i)).collect::<rusqlite::Result<Vec<_>>>())?.collect::<rusqlite::Result<Vec<_>>>()?;
    for row in rows {to.db().execute(&format!("INSERT INTO {table} VALUES({})",vec!["?";count].join(",")),rusqlite::params_from_iter(row))?;}Ok(())
}

impl CoreService {
    pub fn is_temporary(&self)->bool {self.session.lock().ok().is_some_and(|s|s.as_ref().is_some_and(|s|s.temporary.is_some()))}
    pub fn temporary_start(&self)->Result<Value> {
        let _gate=self.chat_gate.lock().map_err(|_|Error::Integrity)?;
        let primary={let session=self.session.lock().map_err(|_|Error::Integrity)?;let session=session.as_ref().ok_or(Error::Locked)?;if session.temporary.is_some(){return Err(Error::Conflict);}session.vault.clone()};
        if self.tasks.lock().map_err(|_|Error::Integrity)?.values().any(|t|t.state.status=="running"){return Err(Error::Conflict);}
        let area=crate::ephemeral::Area::create(&self.services.layout.data_root.join("ephemeral"))?;
        let id=uuid::Uuid::new_v4().simple().to_string();
        let header=vault::VaultHeader{schema_version:"shouyu.ephemeral/1".into(),vault_id:id.clone(),created_at:now_rfc3339(),kdf:vault::KdfRecord{algorithm:"none".into(),params:KdfParams::DESIGN_DEFAULT,salt:String::new()},passphrase_wrap:String::new(),recovery_salt:String::new(),recovery_wrap:String::new(),formats:vault::Formats{database:1,objects:2,index:1}};
        let mut opened=Vault::open(&area.path,header,vault::VaultKeys::ephemeral(&id)?,self.registry()?)?;opened.ephemeral=Some(area);
        {let from=self.lock_vault(&primary)?;
            // Connection refs keep their identity; only their vault scope changes.
            let mut stmt=from.db().prepare("SELECT id,kind,version,active,created_at,parent FROM objects WHERE kind='ConnectionRef'")?;
            for row in stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?)))? {let(a,b,c,d,e,f)=row?;opened.db().execute("INSERT INTO objects(id,scope,kind,version,active,created_at,parent) VALUES(?,?,?,?,?,?,?)",params![a,id,b,c,d,e,f])?;}
            copy_rows(&from,&opened,"connections","")?;copy_rows(&from,&opened,"capabilities","")?;
            let mut stmt=from.db().prepare("SELECT key,value FROM settings WHERE key='sy.privacy_yaml' OR key LIKE 'sy.consent:%' OR key LIKE 'sy.context:%'")?;
            for row in stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?{let(k,v)=row?;opened.set_setting(&k,&v)?;}
        }
        let root=opened.root_matter()?;let handle=Arc::new(Mutex::new(opened));
        {let mut session=self.session.lock().map_err(|_|Error::Integrity)?;let session=session.as_mut().filter(|s|Arc::ptr_eq(&s.vault,&primary)&&s.temporary.is_none()).ok_or(Error::Locked)?;let mut tasks=self.tasks.lock().map_err(|_|Error::Integrity)?;if tasks.values().any(|t|t.state.status=="running"){return Err(Error::Conflict);}session.temporary=Some((handle,Arc::new(AtomicBool::new(false))));tasks.clear();}
        Ok(json!({"temporary":true,"root_matter":root}))
    }
    pub fn temporary_end(&self)->Result<()> {
        let _gate=self.chat_gate.lock().map_err(|_|Error::Integrity)?;
        let (temporary,primary)={let mut current=self.session.lock().map_err(|_|Error::Integrity)?;let session=current.as_mut().ok_or(Error::Locked)?;let Some((handle,cancel))=session.temporary.take()else{return Ok(());};cancel.store(true,Ordering::Release);(handle,session.vault.clone())};
        let tasks={let mut tasks=self.tasks.lock().map_err(|_|Error::Integrity)?;for t in tasks.values(){t.cancel.store(true,Ordering::Release);}std::mem::take(&mut *tasks)};
        self.services.stop_sensitive();
        // Only receipt metadata crosses back. Never prompts, query strings,
        // filenames, responses, mappings, context or memory.
        {let temp=temporary.lock().map_err(|_|Error::Integrity)?;let normal=self.lock_vault(&primary)?;Self::archive_requests(&temp,&normal,None)?;}
        // This runs on the shell's blocking thread. Do not report destruction
        // while an import/parser still owns a VaultHandle and its purpose keys.
        for task in tasks.into_values(){if let Some(thread)=task.thread{let _=thread.join();}}
        drop(temporary);self.tokens.lock().map_err(|_|Error::Integrity)?.clear();self.services.stop_sensitive();Ok(())
    }

    pub(super) fn archive_requests(from:&Vault,to:&Vault,conversation:Option<&str>)->Result<()> {
        let mut stmt=from.db().prepare("SELECT r.id,r.turn_id,r.recipient,r.purpose,CASE WHEN r.status='sent' THEN 'unknown' ELSE r.status END,r.reasoning,r.input_tokens,r.output_tokens,r.error,r.created_at,r.finished_at FROM sy_requests r JOIN sy_turns t ON t.id=r.turn_id WHERE ? IS NULL OR t.conversation_id=?")?;
        let rows=stmt.query_map(params![conversation,conversation],|r|(0..11).map(|i|r.get::<_,rusqlite::types::Value>(i)).collect::<rusqlite::Result<Vec<_>>>())?.collect::<rusqlite::Result<Vec<_>>>()?;
        for row in rows{to.db().execute("INSERT OR IGNORE INTO sy_request_archive VALUES(?,?,?,?,?,?,?,?,?,?,?)",rusqlite::params_from_iter(row))?;}Ok(())
    }
    fn cancel_conversation_tasks(&self,vault:&Vault,id:&str)->Result<()> {
        let mut stmt=vault.db().prepare("SELECT id FROM sy_turns WHERE conversation_id=?")?;let turns=stmt.query_map([id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks=self.tasks.lock().map_err(|_|Error::Integrity)?;
        tasks.retain(|_,t|{if turns.contains(&t.state.root){t.cancel.store(true,Ordering::Release);false}else{true}});Ok(())
    }
    pub(super) fn invalidate_conversation(&self,vault:&Vault,id:&str)->Result<()> {
        self.cancel_conversation_tasks(vault,id)?;
        vault.db().execute("UPDATE sy_conversations SET revision=revision+1,updated_at=? WHERE id=?",params![now_rfc3339(),id])?;
        vault.db().execute("UPDATE sy_turns SET state=CASE WHEN state IN ('preparing','running','review') THEN 'cancelled' ELSE state END,input='{}',prepared=NULL,preview=NULL,context_info=NULL WHERE conversation_id=?",[id])?;
        vault.db().execute("DELETE FROM sy_context WHERE conversation_id=?",[id])?;vault.db().execute("DELETE FROM sy_maps WHERE conversation_id=?",[id])?;
        // Internal export snapshots are derived copies. Explicit external files
        // are outside this database's ownership and aren't silently deleted.
        vault.db().execute("INSERT OR IGNORE INTO sy_blob_gc SELECT e.content_blob FROM exports e WHERE EXISTS(SELECT 1 FROM json_each(e.result_refs) j JOIN results r ON r.id=j.value WHERE r.skill='shouyu-chat' AND r.invocation=?)",[id])?;
        vault.db().execute("DELETE FROM results WHERE id IN(SELECT e.id FROM exports e WHERE EXISTS(SELECT 1 FROM json_each(e.result_refs) j JOIN results r ON r.id=j.value WHERE r.skill='shouyu-chat' AND r.invocation=?))",[id])?;
        vault.db().execute("UPDATE objects SET active=0 WHERE id IN(SELECT e.id FROM exports e WHERE EXISTS(SELECT 1 FROM json_each(e.result_refs) j JOIN results r ON r.id=j.value WHERE r.skill='shouyu-chat' AND r.invocation=?))",[id])?;
        vault.db().execute("DELETE FROM exports WHERE EXISTS(SELECT 1 FROM json_each(exports.result_refs) j JOIN results r ON r.id=j.value WHERE r.skill='shouyu-chat' AND r.invocation=?)",[id])?;
        vault.db().execute("UPDATE objects SET active=0 WHERE id IN(SELECT id FROM results WHERE skill='shouyu-chat' AND invocation=?)",[id])?;
        vault.db().execute("DELETE FROM results WHERE skill='shouyu-chat' AND invocation=?",[id])?;Ok(())
    }
    pub fn chat_manage(&self,id:&str,operation:&str,text:&str,target:Option<&str>,revision:i64)->Result<Value> {
        let _gate=self.chat_gate.lock().map_err(|_|Error::Integrity)?;
        let handle=self.vault()?;let vault=self.lock_vault(&handle)?;vault.expect_kind(id,&["ConversationRef"])?;
        let(branch,current):(String,i64)=vault.db().query_row("SELECT branch,revision FROM sy_conversations WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if current!=revision{return Err(Error::Conflict);}
        if operation=="rename" {nonempty(text,180)?;vault.db().execute("UPDATE sy_conversations SET title=?,title_custom=1,updated_at=? WHERE id=?",params![text.trim(),now_rfc3339(),id])?;return Ok(json!({"id":id}));}
        vault.db().execute_batch("SAVEPOINT sy_manage")?;
        let result=(||->Result<Value>{
            if !matches!(operation,"delete"|"edit_message"|"delete_message"|"switch_branch"){return Err(Error::Contract);}
            self.invalidate_conversation(&vault,id)?;
            match operation {
                "delete"=>{
                    Self::archive_requests(&vault,&vault,Some(id))?;
                    vault.db().execute("DELETE FROM sy_requests WHERE turn_id IN(SELECT id FROM sy_turns WHERE conversation_id=?)",[id])?;
                    vault.db().execute("DELETE FROM sy_tool_audit WHERE turn_id IN(SELECT id FROM sy_turns WHERE conversation_id=?)",[id])?;
                    vault.db().execute("DELETE FROM sy_turns WHERE conversation_id=?",[id])?;
                    vault.db().execute("DELETE FROM sy_branch_messages WHERE branch IN(SELECT id FROM sy_branches WHERE conversation_id=?)",[id])?;
                    vault.db().execute("DELETE FROM sy_branches WHERE conversation_id=?",[id])?;
                    vault.db().execute("DELETE FROM sy_messages WHERE conversation_id=?",[id])?;
                    vault.db().execute("UPDATE sy_memories SET source_conversation=NULL,source_message=NULL WHERE source_conversation=?",[id])?;
                    vault.db().execute("DELETE FROM sy_conversations WHERE id=?",[id])?;
                    vault.db().execute("UPDATE objects SET active=0 WHERE id=?",[id])?;
                },
                "switch_branch"=>{let target=target.ok_or(Error::Contract)?;if !vault.db().query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM sy_branches WHERE id=? AND conversation_id=?)",params![target,id],|r|r.get(0))?{return Err(Error::Denied);}vault.db().execute("UPDATE sy_conversations SET branch=? WHERE id=?",params![target,id])?;},
                "edit_message"=>{
                    nonempty(text,16*1024)?;let target=target.ok_or(Error::Contract)?;
                    let (ordinal,role):(i64,String)=vault.db().query_row("SELECT b.ordinal,m.role FROM sy_branch_messages b JOIN sy_messages m ON m.id=b.message_id WHERE b.branch=? AND m.id=?",params![branch,target],|r|Ok((r.get(0)?,r.get(1)?)))?;
                    let new_branch=uuid::Uuid::new_v4().simple().to_string();let msg=format!("msg_{}",uuid::Uuid::new_v4().simple());let now=now_rfc3339();
                    vault.db().execute("INSERT INTO sy_branches VALUES(?,?,?,?)",params![new_branch,id,format!("编辑分支 · {}",now),now])?;
                    vault.db().execute("INSERT INTO sy_branch_messages SELECT ?,message_id,ordinal FROM sy_branch_messages WHERE branch=? AND ordinal<?",params![new_branch,branch,ordinal])?;
                    vault.db().execute("INSERT INTO sy_messages(id,conversation_id,role,original,status,created_at) VALUES(?,?,?,?,?,?)",params![msg,id,role,text.trim(),if role=="user"{"saved"}else{"complete"},now])?;
                    vault.db().execute("INSERT INTO sy_branch_messages VALUES(?,?,?)",params![new_branch,msg,ordinal])?;
                    vault.db().execute("UPDATE sy_conversations SET branch=? WHERE id=?",params![new_branch,id])?;
                },
                "delete_message"=>{
                    let target=target.ok_or(Error::Contract)?;
                    if !vault.db().query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM sy_branch_messages WHERE branch=? AND message_id=?)",params![branch,target],|r|r.get(0))?{return Err(Error::Denied);}
                    // Remove the selected message and its dependent suffix in
                    // EVERY branch sharing it. No hidden branch can restore it.
                    vault.db().execute("WITH doomed AS MATERIALIZED(SELECT b.rowid FROM sy_branch_messages b JOIN sy_branch_messages anchor ON anchor.branch=b.branch AND b.ordinal>=anchor.ordinal WHERE anchor.message_id=?) DELETE FROM sy_branch_messages WHERE rowid IN(SELECT rowid FROM doomed)",[target])?;
                    vault.db().execute("UPDATE sy_memories SET source_conversation=NULL,source_message=NULL WHERE source_message IN(SELECT id FROM sy_messages WHERE conversation_id=? AND id NOT IN(SELECT message_id FROM sy_branch_messages))",[id])?;
                    vault.db().execute("DELETE FROM sy_messages WHERE conversation_id=? AND id NOT IN(SELECT message_id FROM sy_branch_messages)",[id])?;
                },_=>unreachable!()
            }
            if operation!="delete" {
                let title:Option<String>=vault.db().query_row("SELECT m.original FROM sy_messages m JOIN sy_branch_messages b ON b.message_id=m.id JOIN sy_conversations c ON c.branch=b.branch WHERE c.id=? AND m.role='user' ORDER BY b.ordinal LIMIT 1",[id],|r|r.get(0)).optional()?;
                let title=title.map(|s|s.chars().take(28).collect::<String>()).unwrap_or_else(||"新对话".into());
                vault.db().execute("UPDATE sy_conversations SET title=? WHERE id=? AND title_custom=0",params![title,id])?;
            }
            Ok(json!({"id":id,"deleted":operation=="delete"}))
        })();if result.is_err(){let _=vault.db().execute_batch("ROLLBACK TO sy_manage;RELEASE sy_manage");return result;}vault.db().execute_batch("RELEASE sy_manage")?;vault.collect_chat_garbage()?;result
    }

    pub fn memories(&self)->Result<Vec<Value>> {
        if self.is_temporary(){return Err(Error::Denied);}let handle=self.vault()?;let vault=self.lock_vault(&handle)?;
        let mut stmt=vault.db().prepare("SELECT m.id,m.project_id,m.text,m.source_conversation,m.source_message,m.updated_at,p.title FROM sy_memories m LEFT JOIN matters p ON p.id=m.project_id ORDER BY m.updated_at DESC LIMIT 500")?;
        Ok(stmt.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"project_id":r.get::<_,Option<String>>(1)?,"text":r.get::<_,String>(2)?,"source_conversation":r.get::<_,Option<String>>(3)?,"source_message":r.get::<_,Option<String>>(4)?,"updated_at":r.get::<_,String>(5)?,"project_title":r.get::<_,Option<String>>(6)?})))?.collect::<rusqlite::Result<_>>()?)
    }
    // Shell confirms the exact proposed text/scope. Model output cannot call
    // this API or silently create a lasting preference.
    pub fn memory_save(&self,id:Option<&str>,text:&str,project:Option<&str>,source:Option<(&str,&str)>,expected:Option<&str>)->Result<String> {
        let _gate=self.chat_gate.lock().map_err(|_|Error::Integrity)?;
        if self.is_temporary(){return Err(Error::Denied);}nonempty(text,2048)?;let handle=self.vault()?;let vault=self.lock_vault(&handle)?;
        if let Some(p)=project{vault.expect_kind(p,&["MatterRef"])?;}
        if let Some((c,m))=source{vault.expect_kind(c,&["ConversationRef"])?;if !vault.db().query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM sy_messages WHERE id=? AND conversation_id=?)",params![m,c],|r|r.get(0))?{return Err(Error::Denied);}}
        if let Some(id)=id {let updated:String=vault.db().query_row("SELECT updated_at FROM sy_memories WHERE id=?",[id],|r|r.get(0))?;if Some(updated.as_str())!=expected{return Err(Error::Conflict);}}
        else if vault.db().query_row::<i64,_,_>("SELECT COUNT(*) FROM sy_memories",[],|r|r.get(0))?>=500{return Err(Error::Budget);}
        let id=id.map(str::to_owned).unwrap_or_else(||format!("memory_{}",uuid::Uuid::new_v4().simple()));let now=chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos,true);
        vault.db().execute_batch("SAVEPOINT sy_memory")?;
        let result=(||->Result<()>{
            vault.db().execute("INSERT INTO sy_memories(id,project_id,text,source_conversation,source_message,created_at,updated_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id,text=excluded.text,source_conversation=excluded.source_conversation,source_message=excluded.source_message,updated_at=excluded.updated_at",params![id,project,text.trim(),source.map(|v|v.0),source.map(|v|v.1),now,now])?;
            self.invalidate_memories(&vault)
        })();if result.is_err(){let _=vault.db().execute_batch("ROLLBACK TO sy_memory;RELEASE sy_memory");result?;}vault.db().execute_batch("RELEASE sy_memory")?;vault.collect_chat_garbage()?;Ok(id)
    }
    pub fn memory_delete(&self,id:&str,expected:&str)->Result<()> {
        let _gate=self.chat_gate.lock().map_err(|_|Error::Integrity)?;
        if self.is_temporary(){return Err(Error::Denied);}let handle=self.vault()?;let vault=self.lock_vault(&handle)?;
        vault.db().execute_batch("SAVEPOINT sy_memory")?;
        let result=(||->Result<()>{if vault.db().execute("DELETE FROM sy_memories WHERE id=? AND updated_at=?",params![id,expected])?!=1{return Err(Error::Conflict);}self.invalidate_memories(&vault)})();
        if result.is_err(){let _=vault.db().execute_batch("ROLLBACK TO sy_memory;RELEASE sy_memory");result?;}vault.db().execute_batch("RELEASE sy_memory")?;vault.collect_chat_garbage()?;Ok(())
    }
    fn invalidate_memories(&self,vault:&Vault)->Result<()> {
        let ids={let mut stmt=vault.db().prepare("SELECT id FROM sy_conversations")?;stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?};
        for id in ids{self.invalidate_conversation(vault,&id)?;}Ok(())
    }
}
