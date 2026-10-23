//! Public package application publishes only after every declared document
//! passed the normal import/index pipeline. Historical collections remain.
use super::*;
use crate::{public_knowledge::Package, store::sha256_hex, vault::now_rfc3339};
use rusqlite::{OptionalExtension, params};

pub fn update(ctx:&Ctx,invocation:&Invocation,input:&Value)->Result<EnvelopeParts>{
    let source=text_of(input,"source_selection_id")?;
    let mode=text_of(input,"update_mode")?;
    let (text,hash,preview):(String,String,String)={
        let vault=ctx.lock()?;vault.expect_kind(&source,&["KnowledgeSourceRef"])?;
        vault.db().query_row("SELECT package,sha256,preview FROM knowledge_sources WHERE id=?",[&source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or(Error::Denied)?
    };
    if sha256_hex(text.as_bytes())!=hash{return Err(Error::Integrity);}
    let package=Package::parse(text.as_bytes())?;
    if mode=="preview"{
        let result=ctx.lock()?.put_result(invocation.skill(),invocation.id(),"KnowledgeUpdateResultRef",&serde_json::from_str::<Value>(&preview)?)?;
        return Ok(completed(Some(result)));
    }
    if mode!="apply"{return Err(Error::Contract);}
    ctx.authorize(invocation,"public_knowledge.write")?;
    let action=super::external::claim_action(ctx,invocation,input)?;
    let result=apply(ctx,invocation,&source,&package,&hash);
    super::external::finish_action(ctx,&action,&result)?;
    result
}

fn apply(ctx:&Ctx,invocation:&Invocation,source:&str,package:&Package,hash:&str)->Result<EnvelopeParts>{
    let (package_ref,collection)={
        let vault=ctx.lock()?;
        let existing:Option<(String,String,String,String)>=vault.db().query_row("SELECT id,collection_id,sha256,state FROM knowledge_packages WHERE package_id=? AND version=?",params![package.package_id,package.version],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        if let Some((id,collection,previous_hash,state))=existing{
            if previous_hash!=hash{return Err(Error::Conflict);}
            if state=="active"||state=="superseded"||state=="disabled"{
                let id=vault.put_result(invocation.skill(),invocation.id(),"KnowledgeUpdateResultRef",&json!({"package_ref":id,"collection_id":collection,"package":package.summary(),"status":"already_imported","state":state}))?;
                return Ok(completed(Some(id)));
            }
            // A fresh native approval may resume exactly this package's
            // inactive collection; it cannot widen its immutable content.
            vault.db().execute("UPDATE trusted_artifacts SET root=? WHERE target=?",params![invocation.root_id(),collection])?;
            (id,collection)
        }else{
            let collection=vault.register("PublicKnowledgeCollectionRef","m2-b",None)?;
            let now=now_rfc3339();
            vault.db().execute("INSERT INTO matters(id,title,matter_type,region,status,is_root,created_at,updated_at) VALUES(?,?,'public_knowledge',?,'importing',0,?,?)",params![collection,format!("{} · {}",package.title,package.version),package.region,now,now])?;
            let id=vault.register("KnowledgePackageRef","m2-b",Some(&collection))?;
            vault.db().execute("INSERT INTO knowledge_packages(id,package_id,version,sha256,collection_id,source_id,state,metadata,created_at) VALUES(?,?,?,?,?,?,'pending',?,?)",params![id,package.package_id,package.version,hash,collection,source,package.summary().to_string(),now])?;
            (id,collection)
        }
    };
    ctx.lock()?.db().execute("UPDATE knowledge_sources SET state='applying' WHERE id=?",[source])?;
    let result=(||{
        let mut imported=vec![];
        for (index,doc) in package.documents.iter().enumerate(){
            ctx.check()?;
            ctx.report(invocation,"public_import",index as u64,package.documents.len() as u64);
            let display=format!("{}-{}.txt",doc.id,package.version);
            let artifact={
                let vault=ctx.lock()?;
                let existing:Option<String>=vault.db().query_row("SELECT id FROM trusted_artifacts WHERE target=? AND display_name=?",params![collection,display],|r|r.get(0)).optional()?;
                if let Some(id)=existing{id}else{
                    let id=vault.register("OfflinePackageArtifactRef","m2-b",Some(&collection))?;
                    let text=doc.import_text();
                    let blob=vault.put_blob(&id,"public_import",text.as_bytes())?;
                    vault.db().execute("INSERT INTO trusted_artifacts(id,root,target,display_name,blob,bytes,sha256) VALUES(?,?,?,?,?,?,?)",params![id,invocation.root_id(),collection,display,blob,text.len() as i64,sha256_hex(text.as_bytes())])?;
                    id
                }
            };
            ctx.lock()?.db().execute("UPDATE trusted_artifacts SET root=? WHERE id=? AND target=?",params![invocation.root_id(),artifact,collection])?;
            let (_,envelope)=super::child(ctx,invocation,"document-import",json!({"selection_id":artifact,"target_ref":collection,"import_mode":"copy"}))?;
            if status_of(&envelope)!="completed"{return Err(if status_of(&envelope)=="cancelled"{Error::Cancelled}else{Error::Worker("public_import_incomplete".into())});}
            let reference=data_of(&envelope).ok_or(Error::Integrity)?;
            let payload=ctx.lock()?.get_result(&reference)?.2;
            let version=payload["version_id"].as_str().ok_or(Error::Integrity)?;
            let complete:bool=ctx.lock()?.db().query_row("SELECT EXISTS(SELECT 1 FROM index_jobs WHERE version_id=? AND status='completed' AND completed=total)",[version],|r|r.get(0))?;
            if !complete {
                let (_,indexed)=super::child(ctx,invocation,"knowledge-index",json!({"document_versions":[version],"operation":"build"}))?;
                if status_of(&indexed)!="completed" {return Err(Error::Worker("public_import_incomplete".into()));}
            }
            ctx.lock()?.db().execute("INSERT OR REPLACE INTO legal_sources(version_id,metadata) VALUES(?,?)",params![payload["version_id"].as_str().ok_or(Error::Integrity)?,doc.metadata().to_string()])?;
            imported.push(json!({"document_id":doc.id,"metadata":doc.metadata(),"version_id":payload["version_id"],"import_result":reference}));
        }
        ctx.check()?;
        let mut vault=ctx.lock()?;
        let tx=vault.runtime.ledger.db.transaction()?;
        // Supersede only after the complete new set is queryable. Old scopes
        // and explicit historical evidence still refer to the older version.
        tx.execute("UPDATE knowledge_packages SET state='superseded' WHERE package_id=? AND state='active' AND id<>?",params![package.package_id,package_ref])?;
        tx.execute("UPDATE knowledge_packages SET state='active' WHERE id=?",[&package_ref])?;
        tx.execute("UPDATE matters SET status='active',updated_at=? WHERE id=?",params![now_rfc3339(),collection])?;
        tx.execute("UPDATE knowledge_sources SET state='applied' WHERE id=?",[source])?;
        tx.commit()?;
        let result=vault.put_result(invocation.skill(),invocation.id(),"KnowledgeUpdateResultRef",&json!({"package_ref":package_ref,"collection_id":collection,"package":package.summary(),"sha256":hash,"status":"active","imports":imported,"index_status":"ready","applied_at":now_rfc3339()}))?;
        Ok(completed(Some(result)))
    })();
    if result.is_err(){
        let vault=ctx.lock()?;
        vault.db().execute("UPDATE knowledge_packages SET state='incomplete' WHERE id=? AND state='pending'",[&package_ref])?;
        vault.db().execute("UPDATE knowledge_sources SET state='interrupted' WHERE id=?",[source])?;
    }
    result
}
