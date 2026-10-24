//! knowledge-index, knowledge-search (BM25 + Qwen vectors, two-level RRF),
//! evidence-rerank (Qwen yes/no) and graph-relations (design 9.1, 9.6, 9.9).
use super::*;
use crate::{
    index::{fts_query, search_tokens},
    vault::now_rfc3339,
};
use rusqlite::{OptionalExtension, params, params_from_iter};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

pub const RRF_K: f64 = 60.0;
const PER_LIST: usize = 25;
const MAX_SLOTS: usize = 200;
const RERANK_PAIRS: usize = 20;
const EMBED_BATCH: usize = 16;

fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Load the dense index from its encrypted snapshot, or rebuild it from the
/// encrypted vector baseline in SQLCipher when missing, stale or unreadable.
pub fn ensure_dense(ctx: &Ctx) -> Result<()> {
    let space = ctx.services.layout.space.id.clone();
    let mut vault = ctx.lock()?;
    ctx.check()?;
    crate::partitioned_index::ensure(&mut vault,&space,ctx.cancel)
}

fn rebuild_dense(vault: &mut crate::store::Vault, space: &str, generation: u64,cancel:&std::sync::atomic::AtomicBool) -> Result<()> {
    crate::partitioned_index::refresh(vault,space,generation as i64,cancel)
}

pub fn current_generation(ctx: &Ctx) -> Result<i64> {
    Ok(ctx.lock()?.db().query_row("SELECT generation FROM index_state WHERE id=1", [], |r| r.get(0)).optional()?.unwrap_or(0))
}
pub(crate) fn stable(vault:&crate::store::Vault,generation:i64)->Result<()> {
    let valid:bool=vault.db().query_row("SELECT EXISTS(SELECT 1 FROM index_state WHERE id=1 AND generation=? AND dirty=0)",[generation],|r|r.get(0))?;
    if valid{Ok(())}else{Err(Error::Worker("snapshot_stale".into()))}
}

fn remove_version_chunks(vault: &crate::store::Vault, version: &str) -> Result<usize> {
    let rows: Vec<(i64, String)> = {
        let mut statement = vault.db().prepare("SELECT rowid,id FROM chunks WHERE version_id=?")?;
        let rows = statement.query_map([version], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (rowid, id) in &rows {
        vault.db().execute("DELETE FROM chunk_fts WHERE rowid=?", [rowid])?;
        vault.db().execute("DELETE FROM chunk_vectors WHERE chunk_rowid=?", [rowid])?;
        vault.db().execute("DELETE FROM chunks WHERE rowid=?", [rowid])?;
        vault.db().execute("UPDATE objects SET active=0 WHERE id=?", [id])?;
    }
    Ok(rows.len())
}

/// Remove a version's chunks because its text was replaced (for example media
/// re-decoded). The ANN index is rebuilt from the remaining vectors on next use.
pub(crate) fn drop_version_index(vault: &mut crate::store::Vault, version: &str) -> Result<usize> {
    vault.db().execute("DELETE FROM index_job_chunks WHERE job IN (SELECT id FROM index_jobs WHERE version_id=?)",[version])?;
    vault.db().execute("DELETE FROM index_jobs WHERE version_id=?",[version])?;
    let removed = remove_version_chunks(vault, version)?;
    if removed > 0 {
        vault.db().execute("UPDATE index_state SET dirty=1 WHERE id=1", [])?;
        vault.dense = None;
    }
    Ok(removed)
}

pub fn index(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "index.write")?;
    let _producer=ctx.services.indexing_slots.acquire(ctx.cancel)?;
    let versions = refs_of(input, "document_versions")?;
    let operation = text_of(input, "operation")?;
    if versions.is_empty() || versions.len() > 200 {
        return Err(Error::Contract);
    }
    {
        let vault = ctx.lock()?;
        for version in &versions {
            vault.expect_kind(version, &["DocumentVersionRef"])?;
        }
    }
    let space = ctx.services.layout.space.id.clone();
    let mut totals = vec![];
    if operation == "remove" {
        let mut vault = ctx.lock()?;
        let mut removed = 0;
        for version in &versions {
            removed += remove_version_chunks(&vault, version)?;
            vault.db().execute("DELETE FROM index_job_chunks WHERE job IN (SELECT id FROM index_jobs WHERE version_id=?)",[version])?;
            vault.db().execute("DELETE FROM index_jobs WHERE version_id=?",[version])?;
        }
        let generation = vault.db().query_row("SELECT generation FROM index_state WHERE id=1", [], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64 + 1;
        rebuild_dense(&mut vault, &space, generation,ctx.cancel)?;
        totals.push(json!({"removed_chunks": removed}));
    } else {
        ctx.authorize(invocation, "local.embedding")?;
        ensure_dense(ctx)?;
        for (position, version) in versions.iter().enumerate() {
            ctx.report(invocation, "index", position as u64, versions.len() as u64);
            totals.push(build_version(ctx, invocation, version, &space)?);
        }
        let mut vault = ctx.lock()?;
        let generation = vault.db().query_row("SELECT generation FROM index_state WHERE id=1", [], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64 + 1;
        rebuild_dense(&mut vault, &space, generation,ctx.cancel)?;
    }
    let generation = current_generation(ctx)?;
    let payload = json!({"operation": operation, "versions": versions, "details": totals, "generation": generation, "embedding_space_id": space});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "IndexResultRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.processed = versions;
    Ok(parts)
}

fn build_version(ctx: &Ctx, invocation: &Invocation, version: &str, space: &str) -> Result<Value> {
    let (matter, blocks): (String, Vec<(i64, String, String)>) = {
        let vault = ctx.lock()?;
        let matter: String = vault.db().query_row("SELECT d.matter_id FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?", [version], |r| r.get(0))?;
        let mut statement = vault.db().prepare("SELECT rowid,kind,text FROM blocks WHERE version_id=? ORDER BY ordinal")?;
        let rows = statement.query_map([version], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        (matter, rows.collect::<std::result::Result<_, _>>()?)
    };
    // The content signature covers the exact parser/OCR blocks as well as
    // the locked embedding space and chunking pipeline. Checkpoints of a
    // changed source are never silently reused.
    let signature=crate::store::sha256_hex(&serde_json::to_vec(&json!({"version":version,"blocks":blocks,"space":space,"chunker":"lvjin-chunker/1;600;800;2048;60"}))?);
    let existing:Option<(String,String,String,i64,i64)>={let vault=ctx.lock()?;vault.db().query_row("SELECT id,signature,status,total,completed FROM index_jobs WHERE version_id=?",[version],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?};
    if let Some((_,ref stored,ref status,total,completed))=existing {
        let published:i64=ctx.lock()?.db().query_row("SELECT COUNT(*) FROM chunks c JOIN chunk_vectors v ON v.chunk_rowid=c.rowid JOIN objects o ON o.id=c.id WHERE c.version_id=? AND v.space=? AND o.active=1",params![version,space],|r|r.get(0))?;
        if stored==&signature && status=="completed" && total==completed && published==total {
            return Ok(json!({"version_id":version,"chunks":total,"reused_vectors":completed,"new_vectors":0,"checkpoint":"completed"}));
        }
    }
    let resumable=existing.as_ref().is_some_and(|(_,stored,status,_,_)|stored==&signature && matches!(status.as_str(),"embedding"|"prepared"));
    let job=if resumable {
        let id=existing.as_ref().ok_or(Error::Integrity)?.0.clone();
        ctx.lock()?.db().execute("UPDATE index_jobs SET attempts=attempts+1,last_root=?,updated_at=? WHERE id=?",params![invocation.root_id(),now_rfc3339(),id])?;id
    } else {crate::store::new_ref()};
    if !resumable {
    let header = blocks.iter().find(|(_, kind, _)| kind == "table_header").map(|(_, _, text)| text.clone());
    let prose: Vec<&(i64, String, String)> = blocks.iter().filter(|(_, kind, _)| !matches!(kind.as_str(), "sheet_row" | "table_header")).collect();
    let rows: Vec<&(i64, String, String)> = blocks.iter().filter(|(_, kind, _)| kind == "sheet_row").collect();
    let mut chunks: Vec<(String, String, i64, String, Vec<Value>, i64)> = vec![];
    for (group, mode) in [(prose, "prose"), (rows, "rows")] {
        if group.is_empty() {
            continue;
        }
        let input: Vec<Value> = group.iter().map(|(_, kind, text)| {
            let kind = if kind == "heading" { "heading" } else if kind == "sheet_row" { "row" } else { "paragraph" };
            let mut block = json!({"text": text, "kind": kind});
            if mode == "rows" {
                if let Some(header) = &header {
                    block["header_text"] = json!(header);
                }
            }
            block
        }).collect();
        ctx.check()?;
        let result = ctx.services.local.call(&json!({"op": "chunk_blocks", "blocks": input, "mode": mode}), ctx.cancel, Duration::from_secs(300))?;
        let units = result["units"].as_array().cloned().unwrap_or_default();
        for chunk in result["chunks"].as_array().ok_or(Error::Contract)? {
            let spans: Vec<Value> = chunk["spans"].as_array().cloned().unwrap_or_default().iter().filter_map(|span| {
                let index = span["block"].as_u64()? as usize;
                Some(json!({"block": group.get(index)?.0, "start": span["start"], "end": span["end"]}))
            }).collect();
            let unit = chunk["unit"].as_i64().unwrap_or(0);
            let title = units.iter().find(|u| u["index"].as_i64() == Some(unit)).and_then(|u| u["title"].as_str()).unwrap_or("").to_owned();
            let unit_key = if mode == "rows" { 100_000 + unit } else { unit };
            chunks.push((chunk["text"].as_str().unwrap_or("").to_owned(), chunk["body"].as_str().unwrap_or("").to_owned(), unit_key, title, spans, chunk["token_count"].as_i64().unwrap_or(0)));
        }
    }
    {
        let mut vault = ctx.lock()?;
        ctx.check()?;
        let tx = vault.runtime.ledger.db.transaction()?;
        let now = now_rfc3339();
        tx.execute("DELETE FROM index_job_chunks WHERE job IN (SELECT id FROM index_jobs WHERE version_id=?)",[version])?;
        tx.execute("DELETE FROM index_jobs WHERE version_id=?",[version])?;
        tx.execute("INSERT INTO index_jobs(id,version_id,signature,space,chunker,original_root,last_root,status,total,updated_at) VALUES(?,?,?,?,?,?,?,'prepared',?,?)",params![job,version,signature,space,"lvjin-chunker/1",invocation.root_id(),invocation.root_id(),chunks.len() as i64,now])?;
        for (ordinal, (text, body, unit, title, spans, tokens)) in chunks.iter().enumerate() {
            tx.execute("INSERT INTO index_job_chunks(job,ordinal,payload) VALUES(?,?,?)",params![job,ordinal as i64,json!({"text":text,"body":body,"unit":unit,"title":title,"spans":spans,"tokens":tokens}).to_string()])?;
        }
        tx.commit()?;
    }
    }
    let (total,reused):(i64,i64)={let vault=ctx.lock()?;vault.db().query_row("SELECT total,completed FROM index_jobs WHERE id=?",[&job],|r|Ok((r.get(0)?,r.get(1)?)))?};
    loop {
        ctx.check()?;
        let batch:Vec<(i64,String)>={let vault=ctx.lock()?;let mut stmt=vault.db().prepare("SELECT ordinal,json_extract(payload,'$.text') FROM index_job_chunks WHERE job=? AND vector IS NULL ORDER BY ordinal LIMIT ?")?;
            stmt.query_map(params![job,EMBED_BATCH as i64],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<std::result::Result<_,_>>()?};
        if batch.is_empty(){break;}
        let done:i64=ctx.lock()?.db().query_row("SELECT completed FROM index_jobs WHERE id=?",[&job],|r|r.get(0))?;
        ctx.report(invocation,"embedding",done as u64,total as u64);
        let texts:Vec<&str>=batch.iter().map(|(_,text)|text.as_str()).collect();
        let result = ctx.services.predict("embedding", &json!({"kind": "embedding", "texts": texts, "are_queries": false}), ctx.cancel)?;
        let vectors = result["vectors"].as_array().ok_or(Error::Contract)?;
        if vectors.len() != batch.len() || result["dimensions"] != json!(1024) {
            return Err(Error::Contract);
        }
        let mut vault = ctx.lock()?;
        ctx.check()?;
        let tx=vault.runtime.ledger.db.transaction()?;
        for ((ordinal, _), vector) in batch.iter().zip(vectors) {
            let values: Vec<f32> = vector.as_array().ok_or(Error::Contract)?.iter().map(|v| v.as_f64().unwrap_or(f64::NAN) as f32).collect();
            let bytes=vector_bytes(&values);crate::partitioned_index::vector(&bytes)?;
            tx.execute("UPDATE index_job_chunks SET vector=? WHERE job=? AND ordinal=? AND vector IS NULL",params![bytes,job,ordinal])?;
        }
        tx.execute("UPDATE index_jobs SET status='embedding',completed=(SELECT COUNT(*) FROM index_job_chunks WHERE job=? AND vector IS NOT NULL),updated_at=? WHERE id=?",params![job,now_rfc3339(),job])?;
        tx.commit()?;
        ctx.report(invocation,"embedding",(done+batch.len() as i64) as u64,total as u64);
    }
    ctx.check()?;
    // Publish only a complete set. The previous searchable chunks are intact
    // until this transaction, so cancellation never exposes a partial index.
    {
        let mut vault=ctx.lock()?;ctx.check()?;
        vault.expect_kind(version,&["DocumentVersionRef"])?;
        let current_blocks:Vec<(i64,String,String)>={let mut s=vault.db().prepare("SELECT rowid,kind,text FROM blocks WHERE version_id=? ORDER BY ordinal")?;s.query_map([version],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<std::result::Result<_,_>>()?};
        let current_signature=crate::store::sha256_hex(&serde_json::to_vec(&json!({"version":version,"blocks":current_blocks,"space":space,"chunker":"lvjin-chunker/1;600;800;2048;60"}))?);
        if current_signature!=signature{return Err(Error::Conflict);}
        let complete:i64=vault.db().query_row("SELECT COUNT(*) FROM index_job_chunks WHERE job=? AND vector IS NOT NULL",[&job],|r|r.get(0))?;
        if complete!=total{return Err(Error::Integrity);}
        let scope=vault.id.clone();
        let tx=vault.runtime.ledger.db.transaction()?;
        let old:Vec<i64>={let mut s=tx.prepare("SELECT rowid FROM chunks WHERE version_id=?")?;s.query_map([version],|r|r.get(0))?.collect::<std::result::Result<_,_>>()?};
        // Allocate above the current maximum before deleting old row IDs.
        let mut rowid:i64=tx.query_row("SELECT COALESCE(MAX(rowid),0) FROM chunks",[],|r|r.get(0))?;
        for id in old {
            tx.execute("DELETE FROM chunk_fts WHERE rowid=?",[id])?;tx.execute("DELETE FROM chunk_vectors WHERE chunk_rowid=?",[id])?;
            tx.execute("UPDATE objects SET active=0 WHERE id=(SELECT id FROM chunks WHERE rowid=?)",[id])?;tx.execute("DELETE FROM chunks WHERE rowid=?",[id])?;
        }
        {
            let mut s=tx.prepare("SELECT ordinal,payload,vector FROM index_job_chunks WHERE job=? ORDER BY ordinal")?;
            let mut rows=s.query([&job])?;
            while let Some(row)=rows.next()? {
                ctx.check()?;rowid+=1;let ordinal:i64=row.get(0)?;let p:Value=serde_json::from_str(&row.get::<_,String>(1)?)?;let bytes:Vec<u8>=row.get(2)?;
                crate::partitioned_index::vector(&bytes)?;
                let id=crate::store::new_ref();let partition=format!("{matter}:{}",(rowid-1)/crate::partitioned_index::PARTITION_ROWS as i64);let now=now_rfc3339();
                tx.execute("INSERT INTO objects(id,scope,kind,version,active,created_at,parent) VALUES(?,?,'EvidenceRef',?,1,?,?)",params![id,scope,signature,now,version])?;
                tx.execute("INSERT INTO chunks(rowid,id,version_id,matter_id,ordinal,unit,unit_title,text,body,token_count,spans,chunker,created_at,partition_id) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![rowid,id,version,matter,ordinal,p["unit"].as_i64().unwrap_or(0),p["title"].as_str().unwrap_or(""),p["text"].as_str().unwrap_or(""),p["body"].as_str().unwrap_or(""),p["tokens"].as_i64().unwrap_or(0),p["spans"].to_string(),"lvjin-chunker/1",now,partition])?;
                tx.execute("INSERT INTO chunk_fts(rowid,tokens) VALUES(?,?)",params![rowid,search_tokens(p["text"].as_str().unwrap_or("")).join(" ")])?;
                tx.execute("INSERT INTO chunk_vectors(chunk_rowid,space,vector) VALUES(?,?,?)",params![rowid,space,bytes])?;
            }
        }
        tx.execute("DELETE FROM index_job_chunks WHERE job=?",[&job])?;
        tx.execute("UPDATE index_jobs SET status='completed',completed=total,updated_at=? WHERE id=?",params![now_rfc3339(),job])?;
        tx.execute("UPDATE document_versions SET status='ready',error_code=NULL WHERE id=? AND status IN ('indexing','interrupted','partial') AND EXISTS(SELECT 1 FROM jobs j WHERE j.id=? AND json_extract(j.state,'$.parse_summary.ocr_ok')=1)",params![version,version])?;
        tx.commit()?;
    }
    ctx.report(invocation,"embedding",total as u64,total as u64);
    Ok(json!({"version_id":version,"chunks":total,"reused_vectors":reused,"new_vectors":total-reused,"checkpoint":"published"}))
}

// ----------------------------------------------------------------------- search

pub(crate) struct Scope {
    pub matters: Vec<String>,
    pub versions: Vec<String>,
    pub policy:crate::retrieval_scope::Policy,
}

pub(crate) fn load_scope(ctx: &Ctx, filters: &[String], snapshot: &str) -> Result<(Scope, i64)> {
    let vault = ctx.lock()?;
    let (_, kind, payload) = vault.get_result(snapshot)?;
    if kind != "RetrievalSnapshotRef" {
        return Err(Error::Denied);
    }
    let mut matters: Vec<String> = payload["matters"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let mut versions: Vec<String> = payload["versions"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let policy:crate::retrieval_scope::Policy=serde_json::from_value(if payload["policy"].is_object(){payload["policy"].clone()}else{json!({})})?;
    policy.validate()?;
    let mut no_versions=false;
    for filter in filters {
        let (kind, value): (String, String) = vault.db().query_row("SELECT kind,value FROM filters WHERE id=?", [filter], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
        let ids: Vec<String> = serde_json::from_str(&value)?;
        match kind.as_str() {
            // Filters can only narrow the snapshot scope, never widen it.
            "matter" => matters.retain(|m| ids.contains(m)),
            "version" => {
                if versions.is_empty() {
                    versions = ids;
                } else {
                    versions.retain(|v| ids.contains(v));
                    if versions.is_empty(){no_versions=true;}
                }
            }
            _ => return Err(Error::Contract),
        }
    }
    let candidates:Vec<String>={let mut s=vault.db().prepare("SELECT v.id FROM document_versions v JOIN objects o ON o.id=v.id WHERE o.active=1 ORDER BY v.document_id,v.version_no")?;
        s.query_map([],|r|r.get(0))?.collect::<std::result::Result<_,_>>()?};
    let mut permitted=vec![];
    if !no_versions {for version in candidates {if crate::retrieval_scope::version_allowed(&vault,&version,&matters,&versions,&policy)?{permitted.push(version);}}}
    Ok((Scope { matters, versions:permitted,policy }, payload["generation"].as_i64().unwrap_or(-1)))
}

fn scope_clause(scope: &Scope) -> (String, Vec<String>) {
    let mut clause = format!("c.matter_id IN ({})", vec!["?"; scope.matters.len().max(1)].join(","));
    let mut values: Vec<String> = if scope.matters.is_empty() { vec![String::new()] } else { scope.matters.clone() };
    clause.push_str(&format!(" AND c.version_id IN ({})", vec!["?"; scope.versions.len().max(1)].join(",")));
    values.extend(if scope.versions.is_empty(){vec![String::new()]}else{scope.versions.clone()});
    (clause, values)
}

/// Two-level weighted RRF: per query over its BM25/vector ranks, then across
/// queries with q0 = 0.5 and rewrites sharing 0.5 (q0 alone weighs 1).
pub fn fuse(per_query: &[(String, Vec<i64>, Vec<i64>)]) -> Vec<(i64, f64, Value)> {
    let rewrites = per_query.len().saturating_sub(1);
    let mut scores: BTreeMap<i64, (f64, Vec<Value>)> = BTreeMap::new();
    for (query_id, bm25, vector) in per_query {
        let weight = if rewrites == 0 { 1.0 } else if query_id == "q0" { 0.5 } else { 0.5 / rewrites as f64 };
        let mut local: BTreeMap<i64, (f64, Option<usize>, Option<usize>)> = BTreeMap::new();
        for (rank, id) in bm25.iter().enumerate() {
            let entry = local.entry(*id).or_insert((0.0, None, None));
            entry.0 += 1.0 / (RRF_K + (rank + 1) as f64);
            entry.1 = Some(rank + 1);
        }
        for (rank, id) in vector.iter().enumerate() {
            let entry = local.entry(*id).or_insert((0.0, None, None));
            entry.0 += 1.0 / (RRF_K + (rank + 1) as f64);
            entry.2 = Some(rank + 1);
        }
        for (id, (score, bm, vec)) in local {
            let total = scores.entry(id).or_insert((0.0, vec![]));
            total.0 += weight * score;
            total.1.push(json!({"query_id": query_id, "bm25_rank": bm, "vector_rank": vec, "rrf": score}));
        }
    }
    let mut ordered: Vec<(i64, f64, Value)> = scores.into_iter().map(|(id, (score, detail))| (id, score, json!(detail))).collect();
    // Stable tie-break by chunk row id keeps results reproducible.
    ordered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    ordered.truncate(MAX_SLOTS);
    ordered
}

pub fn search(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "index.read")?;
    ctx.authorize(invocation, "local.embedding")?;
    let queries: Vec<(String, String)> = input["queries"].as_array().ok_or(Error::Contract)?.iter()
        .map(|q| Ok((q["query_id"].as_str().ok_or(Error::Contract)?.to_owned(), q["query"].as_str().ok_or(Error::Contract)?.to_owned())))
        .collect::<Result<_>>()?;
    let filters = refs_of(input, "filter_ids")?;
    let snapshot = text_of(input, "snapshot_id")?;
    ensure_dense(ctx)?;
    let (scope, generation) = load_scope(ctx, &filters, &snapshot)?;
    if generation != current_generation(ctx)? {
        return Err(Error::Worker("snapshot_stale".into()));
    }
    {
        let mut vault = ctx.lock()?;
        vault.runtime.dispatch_retrieval_round(invocation, &format!("{}:round", invocation.id()))?;
    }
    let per_list=if scope.policy.graph_enabled {PER_LIST.min((MAX_SLOTS-40)/(queries.len()*2))}else{PER_LIST};
    let (clause, values) = scope_clause(&scope);
    let (allowed,partitions): (BTreeSet<u64>,Vec<String>) = {
        let vault = ctx.lock()?;
        stable(&vault,generation)?;
        let _query_cancel=crate::index::QueryCancellation::new(vault.db(),ctx.cancel);
        // Build the ANN filter and partition list in the same scoped scan;
        // scanning and joining 100k encrypted rows twice adds avoidable work.
        let sql = format!("SELECT c.rowid,c.partition_id FROM chunks c JOIN objects o ON o.id=c.id WHERE o.active=1 AND {clause}");
        let mut statement = vault.db().prepare(&sql)?;
        let mut rows=statement.query(params_from_iter(values.iter()))?;
        let mut allowed=BTreeSet::new();let mut partitions=BTreeSet::new();
        let gathered=(||->Result<()>{while let Some(row)=rows.next()? {allowed.insert(row.get::<_,i64>(0)? as u64);partitions.insert(row.get::<_,String>(1)?);}Ok(())})();
        ctx.check()?;gathered?;
        (allowed,partitions.into_iter().collect())
    };
    let mut per_query = vec![];
    let mut detail = vec![];
    for (query_id, text) in &queries {
        ctx.check()?;
        let bm25: Vec<i64> = match fts_query(&search_tokens(text)) {
            Some(expression) => {
                let vault = ctx.lock()?;
                stable(&vault,generation)?;
                let _query_cancel=crate::index::QueryCancellation::new(vault.db(),ctx.cancel);
                // FTS must be the outer loop. With a large single-matter
                // scope SQLite otherwise chooses chunks_matter first and
                // restarts the MATCH cursor once per candidate row.
                let sql = format!("SELECT c.rowid FROM chunk_fts CROSS JOIN chunks c ON c.rowid=chunk_fts.rowid CROSS JOIN objects o ON o.id=c.id WHERE chunk_fts MATCH ? AND o.active=1 AND {clause} ORDER BY bm25(chunk_fts), c.rowid LIMIT {per_list}");
                let mut statement = vault.db().prepare(&sql)?;
                let mut parameters = vec![expression];
                parameters.extend(values.iter().cloned());
                let rows = statement.query_map(params_from_iter(parameters.iter()), |r| r.get::<_, i64>(0))?;
                let collected=rows.collect::<std::result::Result<_, _>>();
                ctx.check()?;
                collected?
            }
            None => vec![],
        };
        let vector: Vec<i64> = if allowed.is_empty() {
            vec![]
        } else {
            let embedded = ctx.services.predict("embedding", &json!({"kind": "embedding", "texts": [text], "are_queries": true}), ctx.cancel)?;
            let query: Vec<f32> = embedded["vectors"][0].as_array().ok_or(Error::Contract)?.iter().map(|v| v.as_f64().unwrap_or(f64::NAN) as f32).collect();
            let mut vault = ctx.lock()?;
            stable(&vault,generation)?;
            crate::partitioned_index::search(&mut vault,&ctx.services.layout.space.id,&query,&allowed,&partitions,per_list,ctx.cancel)?.into_iter().map(|(key, _)|key as i64).collect()
        };
        detail.push(json!({"query_id": query_id, "bm25_hits": bm25.len(), "vector_hits": vector.len()}));
        per_query.push((query_id.clone(), bm25, vector));
    }
    let fused = fuse(&per_query);
    let base_slots:usize=per_query.iter().map(|(_,bm,ann)|bm.len()+ann.len()).sum();
    let candidates: Vec<Value> = {
        let vault = ctx.lock()?;
        stable(&vault,generation)?;
        fused.iter().map(|(rowid, score, detail)| {
            let (id, version, matter): (String, String, String) = vault.db().query_row("SELECT id,version_id,matter_id FROM chunks WHERE rowid=?", [rowid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(json!({"evidence_id": id, "version_id": version, "matter_id": matter, "rrf": score, "ranks": detail}))
        }).collect::<Result<_>>()?
    };
    let payload = json!({"queries": queries.iter().map(|(id, text)| json!({"query_id": id, "query": text})).collect::<Vec<_>>(), "snapshot_id": snapshot,
        "generation": generation, "scope": {"matters": scope.matters, "versions": scope.versions},"policy":scope.policy,"graph_enabled":scope.policy.graph_enabled,"candidate_slots":base_slots,"graph_budget":if scope.policy.graph_enabled{40.min(MAX_SLOTS.saturating_sub(base_slots))}else{0}, "fusion": {"method": "two-level weighted RRF", "k": RRF_K, "per_list": per_list, "max_slots": MAX_SLOTS},
        "per_query": detail, "candidates": candidates});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "CandidateSetRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.evidence_refs = candidates.iter().take(10).filter_map(|c| c["evidence_id"].as_str().map(str::to_owned)).collect();
    if candidates.is_empty() {
        warn(&mut parts, "no_candidates", "在所选范围内没有检索到相关原文。");
    }
    if queries.len() == 1 {
        parts.limitations.push("仅使用原始查询 q0；查询改写需要生成接口".into());
    }
    Ok(parts)
}

pub fn rerank(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "local.rerank")?;
    let question_ref = text_of(input, "query_ref")?;
    let candidate_set = text_of(input, "candidate_set_id")?;
    if !refs_of(input, "facet_ids")?.is_empty() {
        return Err(Error::Denied);
    }
    let (question, candidates) = {
        let vault = ctx.lock()?;
        let question: String = vault.db().query_row("SELECT text FROM questions WHERE id=?", [&question_ref], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
        let (_, kind, payload) = vault.get_result(&candidate_set)?;
        if kind != "CandidateSetRef" {
            return Err(Error::Denied);
        }
        (question, payload["candidates"].as_array().cloned().unwrap_or_default())
    };
    let top: Vec<Value> = candidates.into_iter().take(RERANK_PAIRS).collect();
    let mut items = vec![];
    let mut backend=json!({"device":"cpu","profile":ctx.services.key("reranker")?.profile});
    if !top.is_empty() {
        let texts: Vec<String> = {
            let vault = ctx.lock()?;
            top.iter().map(|c| {
                let id = c["evidence_id"].as_str().ok_or(Error::Contract)?;
                vault.db().query_row("SELECT c.text FROM chunks c JOIN objects o ON o.id=c.id WHERE c.id=? AND o.active=1", [id], |r| r.get::<_, String>(0)).optional()?.ok_or(Error::Denied)
            }).collect::<Result<_>>()?
        };
        ctx.check()?;
        let scored = ctx.services.predict("reranker", &json!({"kind": "reranker", "question": question, "documents": texts}), ctx.cancel)?;
        if !scored["backend"].is_null() {backend=scored["backend"].clone();}
        let scores = scored["scores"].as_array().ok_or(Error::Contract)?;
        if scores.len() != top.len() {
            return Err(Error::Contract);
        }
        for (rank, (candidate, score)) in top.iter().zip(scores).enumerate() {
            items.push(json!({"evidence_id": candidate["evidence_id"], "version_id": candidate["version_id"], "matter_id": candidate["matter_id"],
                "rerank_score": score, "rrf_rank": rank + 1, "rrf": candidate["rrf"]}));
        }
        items.sort_by(|a, b| b["rerank_score"].as_f64().partial_cmp(&a["rerank_score"].as_f64()).unwrap_or(std::cmp::Ordering::Equal).then(a["rrf_rank"].as_u64().cmp(&b["rrf_rank"].as_u64())));
    }
    let strong = items.iter().filter(|i| i["rerank_score"].as_f64().unwrap_or(0.0) >= 0.5).count();
    let take = strong.clamp(6, 10).min(items.len());
    let payload = json!({"question_ref": question_ref, "candidate_set_id": candidate_set, "model": "Qwen3-Reranker-0.6B yes/no", "backend":backend,"pairs": items.len(), "initial_take": take, "items": items});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "EvidenceBundleRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.evidence_refs = items.iter().take(take).filter_map(|i| i["evidence_id"].as_str().map(str::to_owned)).collect();
    Ok(parts)
}

pub fn graph(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    super::graph::run(ctx,invocation,input)
}
