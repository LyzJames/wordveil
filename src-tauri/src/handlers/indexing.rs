//! knowledge-index, knowledge-search (BM25 + Qwen vectors, two-level RRF),
//! evidence-rerank (Qwen yes/no) and graph-relations (design 9.1, 9.6, 9.9).
use super::*;
use crate::{
    index::{DenseIndex, IndexPrecision, SnapshotIdentity, fts_query, search_tokens},
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

fn vector_from(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.len() != 4096 {
        return Err(Error::Integrity);
    }
    Ok(bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

fn identity(vault: &crate::store::Vault, space: &str, generation: u64) -> SnapshotIdentity {
    SnapshotIdentity { scope: vault.id.clone(), generation, embedding_space_id: space.into(), precision: IndexPrecision::F32 }
}

/// Load the dense index from its encrypted snapshot, or rebuild it from the
/// encrypted vector baseline in SQLCipher when missing, stale or unreadable.
pub fn ensure_dense(ctx: &Ctx) -> Result<()> {
    let space = ctx.services.layout.space.id.clone();
    let mut vault = ctx.lock()?;
    if vault.dense.as_ref().is_some_and(|d| d.identity().embedding_space_id == space) {
        return Ok(());
    }
    let state: Option<(i64, String, Option<String>, i64)> =
        vault.db().query_row("SELECT generation,space,snapshot_blob,dirty FROM index_state WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
    if let Some((generation, stored_space, Some(blob), 0)) = state.clone() {
        if stored_space == space {
            let envelope = vault.get_blob(&blob)?;
            if let Ok(index) = DenseIndex::reopen(identity(&vault, &space, generation as u64), &vault.keys.index, &envelope) {
                vault.dense = Some(index);
                return Ok(());
            }
        }
    }
    let generation = state.map(|s| s.0).unwrap_or(0) as u64 + 1;
    rebuild_dense(&mut vault, &space, generation)
}

fn rebuild_dense(vault: &mut crate::store::Vault, space: &str, generation: u64) -> Result<()> {
    let rows: Vec<(i64, Vec<u8>)> = {
        let mut statement = vault.db().prepare("SELECT v.chunk_rowid,v.vector FROM chunk_vectors v JOIN chunks c ON c.rowid=v.chunk_rowid JOIN objects o ON o.id=c.id WHERE o.active=1 AND v.space=?")?;
        let rows = statement.query_map([space], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let index = DenseIndex::new(identity(vault, space, generation), rows.len().max(16))?;
    for (rowid, bytes) in &rows {
        index.insert(*rowid as u64, &vector_from(bytes)?)?;
    }
    vault.dense = Some(index);
    persist_dense(vault, space, generation)
}

/// Seal the in-memory index (no plaintext file, no mmap) and swap snapshots.
fn persist_dense(vault: &mut crate::store::Vault, space: &str, generation: u64) -> Result<()> {
    let index = vault.dense.as_ref().ok_or(Error::Integrity)?;
    let envelope = index.encrypted_snapshot(&vault.keys.index)?;
    let count = index.len() as i64;
    let blob = vault.put_blob("index", "index_snapshot", &envelope)?;
    let previous: Option<String> = vault.db().query_row("SELECT snapshot_blob FROM index_state WHERE id=1", [], |r| r.get(0)).optional()?.flatten();
    vault.db().execute(
        "INSERT INTO index_state(id,generation,space,snapshot_blob,vector_count,dirty,updated_at) VALUES(1,?,?,?,?,0,?)
         ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,space=excluded.space,snapshot_blob=excluded.snapshot_blob,vector_count=excluded.vector_count,dirty=0,updated_at=excluded.updated_at",
        params![generation as i64, space, blob, count, now_rfc3339()],
    )?;
    if let Some(previous) = previous {
        if previous != blob {
            vault.delete_blob(&previous)?;
        }
    }
    Ok(())
}

pub fn current_generation(ctx: &Ctx) -> Result<i64> {
    Ok(ctx.lock()?.db().query_row("SELECT generation FROM index_state WHERE id=1", [], |r| r.get(0)).optional()?.unwrap_or(0))
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
    let removed = remove_version_chunks(vault, version)?;
    if removed > 0 {
        vault.db().execute("UPDATE index_state SET dirty=1 WHERE id=1", [])?;
        vault.dense = None;
    }
    Ok(removed)
}

pub fn index(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "index.write")?;
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
        }
        let generation = vault.db().query_row("SELECT generation FROM index_state WHERE id=1", [], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64 + 1;
        rebuild_dense(&mut vault, &space, generation)?;
        totals.push(json!({"removed_chunks": removed}));
    } else {
        ctx.authorize(invocation, "local.embedding")?;
        ensure_dense(ctx)?;
        let mut removed = 0;
        let mut additions: Vec<(i64, Vec<f32>)> = vec![];
        for (position, version) in versions.iter().enumerate() {
            ctx.report(invocation, "index", position as u64, versions.len() as u64);
            let (summary, dropped, vectors) = build_version(ctx, invocation, version, &space)?;
            removed += dropped;
            additions.extend(vectors);
            totals.push(summary);
        }
        let mut vault = ctx.lock()?;
        let generation = vault.db().query_row("SELECT generation FROM index_state WHERE id=1", [], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64 + 1;
        if removed == 0 && vault.dense.is_some() {
            let dense = vault.dense.as_mut().ok_or(Error::Integrity)?;
            dense.set_generation(generation)?;
            for (rowid, vector) in &additions {
                dense.insert(*rowid as u64, vector)?;
            }
            persist_dense(&mut vault, &space, generation)?;
        } else {
            // Replaced vectors are dropped by a rebuild so no snapshot retains them.
            rebuild_dense(&mut vault, &space, generation)?;
        }
    }
    let generation = current_generation(ctx)?;
    let payload = json!({"operation": operation, "versions": versions, "details": totals, "generation": generation, "embedding_space_id": space});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "IndexResultRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.processed = versions;
    Ok(parts)
}

fn build_version(ctx: &Ctx, invocation: &Invocation, version: &str, space: &str) -> Result<(Value, usize, Vec<(i64, Vec<f32>)>)> {
    let (matter, blocks): (String, Vec<(i64, String, String)>) = {
        let vault = ctx.lock()?;
        let matter: String = vault.db().query_row("SELECT d.matter_id FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?", [version], |r| r.get(0))?;
        let mut statement = vault.db().prepare("SELECT rowid,kind,text FROM blocks WHERE version_id=? ORDER BY ordinal")?;
        let rows = statement.query_map([version], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        (matter, rows.collect::<std::result::Result<_, _>>()?)
    };
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
    let version_label = result_version(&chunks);
    let mut inserted: Vec<(i64, String)> = vec![];
    let mut vectors_out: Vec<(i64, Vec<f32>)> = vec![];
    let removed;
    {
        let mut vault = ctx.lock()?;
        let scope = vault.id.clone();
        removed = remove_version_chunks(&vault, version)?;
        let tx = vault.runtime.ledger.db.transaction()?;
        let now = now_rfc3339();
        for (ordinal, (text, body, unit, title, spans, tokens)) in chunks.iter().enumerate() {
            let id = crate::store::new_ref();
            tx.execute("INSERT INTO objects(id,scope,kind,version,active,created_at,parent) VALUES(?,?,'EvidenceRef',?,1,?,?)", params![id, scope, version_label, now, version])?;
            tx.execute(
                "INSERT INTO chunks(id,version_id,matter_id,ordinal,unit,unit_title,text,body,token_count,spans,chunker,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                params![id, version, matter, ordinal as i64, unit, title, text, body, tokens, serde_json::to_string(spans)?, "lvjin-chunker/1", now],
            )?;
            let rowid = tx.last_insert_rowid();
            tx.execute("INSERT INTO chunk_fts(rowid,tokens) VALUES(?,?)", params![rowid, search_tokens(text).join(" ")])?;
            inserted.push((rowid, text.clone()));
        }
        tx.commit()?;
    }
    for (position, batch) in inserted.chunks(EMBED_BATCH).enumerate() {
        ctx.check()?;
        ctx.report(invocation, "embedding", (position * EMBED_BATCH) as u64, inserted.len() as u64);
        let texts: Vec<&str> = batch.iter().map(|(_, text)| text.as_str()).collect();
        let result = ctx.services.predict("embedding", &json!({"kind": "embedding", "texts": texts, "are_queries": false}), ctx.cancel)?;
        let vectors = result["vectors"].as_array().ok_or(Error::Contract)?;
        if vectors.len() != batch.len() || result["dimensions"] != json!(1024) {
            return Err(Error::Contract);
        }
        let vault = ctx.lock()?;
        for ((rowid, _), vector) in batch.iter().zip(vectors) {
            let values: Vec<f32> = vector.as_array().ok_or(Error::Contract)?.iter().map(|v| v.as_f64().unwrap_or(f64::NAN) as f32).collect();
            if values.len() != 1024 || values.iter().any(|v| !v.is_finite()) {
                return Err(Error::Contract);
            }
            vault.db().execute("INSERT OR REPLACE INTO chunk_vectors(chunk_rowid,space,vector) VALUES(?,?,?)", params![rowid, space, vector_bytes(&values)])?;
            vectors_out.push((*rowid, values));
        }
    }
    Ok((json!({"version_id": version, "chunks": inserted.len()}), removed, vectors_out))
}

fn result_version(chunks: &[(String, String, i64, String, Vec<Value>, i64)]) -> String {
    format!("chunks:{}", chunks.len())
}

// ----------------------------------------------------------------------- search

struct Scope {
    matters: Vec<String>,
    versions: Vec<String>,
}

fn load_scope(ctx: &Ctx, filters: &[String], snapshot: &str) -> Result<(Scope, i64)> {
    let vault = ctx.lock()?;
    let (_, kind, payload) = vault.get_result(snapshot)?;
    if kind != "RetrievalSnapshotRef" {
        return Err(Error::Denied);
    }
    let mut matters: Vec<String> = payload["matters"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let mut versions: Vec<String> = payload["versions"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
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
                }
            }
            _ => return Err(Error::Contract),
        }
    }
    Ok((Scope { matters, versions }, payload["generation"].as_i64().unwrap_or(-1)))
}

fn scope_clause(scope: &Scope) -> (String, Vec<String>) {
    let mut clause = format!("c.matter_id IN ({})", vec!["?"; scope.matters.len().max(1)].join(","));
    let mut values: Vec<String> = if scope.matters.is_empty() { vec![String::new()] } else { scope.matters.clone() };
    if !scope.versions.is_empty() {
        clause.push_str(&format!(" AND c.version_id IN ({})", vec!["?"; scope.versions.len()].join(",")));
        values.extend(scope.versions.iter().cloned());
    }
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
    let (scope, generation) = load_scope(ctx, &filters, &snapshot)?;
    if generation != current_generation(ctx)? {
        return Err(Error::Worker("snapshot_stale".into()));
    }
    {
        let mut vault = ctx.lock()?;
        vault.runtime.dispatch_retrieval_round(invocation, &format!("{}:round", invocation.id()))?;
    }
    ensure_dense(ctx)?;
    let (clause, values) = scope_clause(&scope);
    let allowed: BTreeSet<u64> = {
        let vault = ctx.lock()?;
        let sql = format!("SELECT c.rowid FROM chunks c JOIN objects o ON o.id=c.id WHERE o.active=1 AND {clause}");
        let mut statement = vault.db().prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values.iter()), |r| r.get::<_, i64>(0))?;
        rows.map(|r| r.map(|v| v as u64)).collect::<std::result::Result<_, _>>()?
    };
    let mut per_query = vec![];
    let mut detail = vec![];
    for (query_id, text) in &queries {
        ctx.check()?;
        let bm25: Vec<i64> = match fts_query(&search_tokens(text)) {
            Some(expression) => {
                let vault = ctx.lock()?;
                let sql = format!("SELECT c.rowid FROM chunk_fts JOIN chunks c ON c.rowid=chunk_fts.rowid JOIN objects o ON o.id=c.id WHERE chunk_fts MATCH ? AND o.active=1 AND {clause} ORDER BY bm25(chunk_fts), c.rowid LIMIT {PER_LIST}");
                let mut statement = vault.db().prepare(&sql)?;
                let mut parameters = vec![expression];
                parameters.extend(values.iter().cloned());
                let rows = statement.query_map(params_from_iter(parameters.iter()), |r| r.get::<_, i64>(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            }
            None => vec![],
        };
        let vector: Vec<i64> = if allowed.is_empty() {
            vec![]
        } else {
            let embedded = ctx.services.predict("embedding", &json!({"kind": "embedding", "texts": [text], "are_queries": true}), ctx.cancel)?;
            let query: Vec<f32> = embedded["vectors"][0].as_array().ok_or(Error::Contract)?.iter().map(|v| v.as_f64().unwrap_or(f64::NAN) as f32).collect();
            let vault = ctx.lock()?;
            let dense = vault.dense.as_ref().ok_or(Error::Integrity)?;
            let identity = dense.identity().clone();
            dense.search_within(&identity, &query, PER_LIST, |key| allowed.contains(&key))?.into_iter().map(|(key, _)| key as i64).collect()
        };
        detail.push(json!({"query_id": query_id, "bm25_hits": bm25.len(), "vector_hits": vector.len()}));
        per_query.push((query_id.clone(), bm25, vector));
    }
    let fused = fuse(&per_query);
    let candidates: Vec<Value> = {
        let vault = ctx.lock()?;
        fused.iter().map(|(rowid, score, detail)| {
            let (id, version, matter): (String, String, String) = vault.db().query_row("SELECT id,version_id,matter_id FROM chunks WHERE rowid=?", [rowid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(json!({"evidence_id": id, "version_id": version, "matter_id": matter, "rrf": score, "ranks": detail}))
        }).collect::<Result<_>>()?
    };
    let payload = json!({"queries": queries.iter().map(|(id, text)| json!({"query_id": id, "query": text})).collect::<Vec<_>>(), "snapshot_id": snapshot,
        "generation": generation, "scope": {"matters": scope.matters, "versions": scope.versions}, "fusion": {"method": "two-level weighted RRF", "k": RRF_K, "per_list": PER_LIST, "max_slots": MAX_SLOTS},
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
    let payload = json!({"question_ref": question_ref, "candidate_set_id": candidate_set, "model": "Qwen3-Reranker-0.6B yes/no", "pairs": items.len(), "initial_take": take, "items": items});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "EvidenceBundleRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.evidence_refs = items.iter().take(take).filter_map(|i| i["evidence_id"].as_str().map(str::to_owned)).collect();
    Ok(parts)
}

pub fn graph(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "graph.read")?;
    let seeds = refs_of(input, "seed_ids")?;
    let types: Vec<String> = input["relation_types"].as_array().ok_or(Error::Contract)?.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let hops = input["max_hops"].as_u64().ok_or(Error::Contract)?.min(2);
    let vault = ctx.lock()?;
    let mut frontier: BTreeSet<String> = seeds.iter().cloned().collect();
    let mut seen = frontier.clone();
    let mut edges: Vec<Value> = vec![];
    for _ in 0..hops {
        let mut next = BTreeSet::new();
        for node in &frontier {
            let mut statement = vault.db().prepare(
                "SELECT r.id,r.src,r.dst,r.relation_type,r.evidence,r.status,r.origin FROM relations r JOIN objects o ON o.id=r.id WHERE o.active=1 AND (r.src=? OR r.dst=?)",
            )?;
            let rows = statement.query_map(params![node, node], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?)))?;
            for row in rows {
                let (id, src, dst, kind, evidence, status, origin) = row?;
                if !types.is_empty() && !types.contains(&kind) {
                    continue;
                }
                if edges.iter().any(|e| e["id"] == json!(id)) {
                    continue;
                }
                // Direction is preserved; traversal in both directions does not flip it.
                edges.push(json!({"id": id, "src": src, "dst": dst, "relation_type": kind, "evidence": evidence.and_then(|e| serde_json::from_str::<Value>(&e).ok()), "status": status, "origin": origin}));
                for other in [src, dst] {
                    if seen.insert(other.clone()) {
                        next.insert(other);
                    }
                }
                if edges.len() >= MAX_SLOTS {
                    break;
                }
            }
        }
        frontier = next;
        if frontier.is_empty() || edges.len() >= MAX_SLOTS {
            break;
        }
    }
    let nodes: Vec<Value> = seen.iter().map(|id| json!({"id": id, "kind": vault.object_kind(id).ok()})).collect();
    let payload = json!({"seeds": seeds, "max_hops": hops, "relation_types": types, "nodes": nodes, "edges": edges});
    drop(vault);
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "GraphResultRef", &payload)?;
    Ok(completed(Some(result)))
}
