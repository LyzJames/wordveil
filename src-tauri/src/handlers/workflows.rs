//! Composite workflows without a generation connection (design 7.1, 9.1):
//! lvjin-retrieval delivers fixed-q0 hybrid retrieval, rerank and exact source
//! reading; contract/loan/budget analyses deliver their local evidence and
//! deterministic calculations and then report `requires_generation`.
use super::*;
use crate::vault::now_rfc3339;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// Core-created retrieval scope (matters and optional versions).
pub fn create_scope(ctx: &Ctx, matters: &[String], versions: &[String]) -> Result<String> {
    let vault = ctx.lock()?;
    for matter in matters {
        vault.expect_kind(matter, &["MatterRef"])?;
    }
    for version in versions {
        vault.expect_kind(version, &["DocumentVersionRef"])?;
    }
    let id = vault.register("ScopeRef", "m1", None)?;
    vault.db().execute("INSERT INTO scopes(id,matters,versions,created_at) VALUES(?,?,?,?)", params![id, serde_json::to_string(matters)?, serde_json::to_string(versions)?, now_rfc3339()])?;
    Ok(id)
}

fn scope_contents(ctx: &Ctx, scope: &str) -> Result<(Vec<String>, Vec<String>)> {
    let (matters, versions): (String, String) = ctx.lock()?.db().query_row("SELECT matters,versions FROM scopes WHERE id=?", [scope], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
    Ok((serde_json::from_str(&matters)?, serde_json::from_str(&versions)?))
}

pub fn retrieval(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let question = text_of(input, "question_id")?;
    let scope = text_of(input, "scope_id")?;
    let constraints = refs_of(input, "known_constraint_ids")?;
    let requested = text_of(input, "requested_output")?;
    let text: String = ctx.lock()?.db().query_row("SELECT text FROM questions WHERE id=?", [&question], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
    let (matters, versions) = scope_contents(ctx, &scope)?;
    let generation = super::indexing::current_generation(ctx)?;
    let snapshot = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "RetrievalSnapshotRef", &json!({"generation": generation, "matters": matters, "versions": versions, "scope_id": scope}))?;
    let mut parts = completed(None);
    // Without a generation connection the fixed original query q0 is used
    // (design 9.1); query-plan is not invoked and no rewrite is fabricated.
    let (_, search) = child(ctx, invocation, "knowledge-search", json!({"queries": [{"query_id": "q0", "query": text}], "filter_ids": [], "snapshot_id": snapshot}))?;
    if status_of(&search) != "completed" {
        parts.warnings.extend(child_warnings(&search));
        if status_of(&search) != "partial" {
            parts.status = "partial".into();
        }
    }
    let candidate_set = data_of(&search);
    let mut bundle = None;
    let mut read = None;
    let mut evidence = vec![];
    if let Some(set) = &candidate_set {
        let (_, reranked) = child(ctx, invocation, "evidence-rerank", json!({"query_ref": question, "candidate_set_id": set, "facet_ids": []}))?;
        bundle = data_of(&reranked);
        evidence = reranked["evidence_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        if status_of(&reranked) != "completed" {
            parts.status = "partial".into();
            parts.warnings.extend(child_warnings(&reranked));
        }
        if !evidence.is_empty() {
            let (_, reading) = child(ctx, invocation, "evidence-read", json!({"evidence_ids": evidence, "context_mode": "exact"}))?;
            read = data_of(&reading);
        }
    }
    let read_payload = match &read {
        Some(id) => ctx.lock()?.get_result(id)?.2,
        None => json!({"items": []}),
    };
    let payload = json!({"question_id": question, "question": text, "scope": {"matters": matters, "versions": versions}, "known_constraint_ids": constraints,
        "candidate_set": candidate_set, "evidence_bundle": bundle, "evidence_read": read, "items": read_payload["items"], "requested_output": requested,
        "query_rewrites": [], "notes": "仅原始查询 q0；未连接生成接口，未生成分析答案"});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "RetrievalResultRef", &payload)?;
    parts.data_ref = Some(result);
    parts.evidence_refs = evidence;
    if parts.evidence_refs.is_empty() {
        warn(&mut parts, "insufficient_evidence", "依据不足：所选范围内没有检索到相关原文。");
    }
    if requested == "answer" {
        parts.status = "requires_generation".into();
        warn(&mut parts, "requires_generation", "生成答案需要配置生成接口；已交付本地检索到的原文依据。");
    }
    Ok(parts)
}

fn matter_of_version(ctx: &Ctx, version: &str) -> Result<String> {
    ctx.lock()?.db().query_row("SELECT d.matter_id FROM document_versions v JOIN documents d ON d.id=v.document_id JOIN objects o ON o.id=v.id WHERE v.id=? AND o.active=1", [version], |r| r.get(0)).optional()?.ok_or(Error::Denied)
}

fn confirmed_cashflows(ctx: &Ctx, matter: &str, kinds: &[&str], accounts: &[String], period: Option<(&str, &str)>) -> Result<Vec<String>> {
    let vault = ctx.lock()?;
    let mut statement = vault.db().prepare("SELECT c.id,c.kind,c.account_id,c.occurred_on FROM cashflows c JOIN objects o ON o.id=c.id WHERE c.matter_id=? AND c.status='confirmed' AND o.active=1 ORDER BY c.occurred_on")?;
    let rows = statement.query_map([matter], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?)))?;
    let mut output = vec![];
    for row in rows {
        let (id, kind, account, date) = row?;
        if !kinds.is_empty() && !kinds.contains(&kind.as_str()) {
            continue;
        }
        if !accounts.is_empty() && !account.as_ref().is_some_and(|a| accounts.contains(a)) {
            continue;
        }
        if let Some((start, end)) = period {
            if date.as_str() < start || date.as_str() > end {
                continue;
            }
        }
        output.push(id);
    }
    Ok(output)
}

pub fn analysis(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let skill = invocation.skill().to_owned();
    let question = text_of(input, "question_id")?;
    ctx.lock()?.expect_kind(&question, &["ImmutableQuestionRef"])?;
    let versions: Vec<String> = input["document_versions"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let media: Vec<String> = input["media_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let mut parts = completed(None);
    let mut calculation = None;
    let mut media_results = vec![];
    for media_ref in &media {
        // Media become locatable text evidence first (text_only; native review is M2).
        let (kind, version): (String, String) = ctx.lock()?.db().query_row("SELECT v.media_kind,v.id FROM media m JOIN document_versions v ON v.id=m.version_id WHERE m.id=?", [media_ref], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
        let range = {
            let vault = ctx.lock()?;
            let id = vault.register("ApprovedTimeRangeRef", "m1", Some(media_ref))?;
            vault.db().execute("INSERT INTO ranges(id,media_id,start_s,end_s,created_at) VALUES(?,?,0,NULL,?)", params![id, media_ref, now_rfc3339()])?;
            id
        };
        let skill_name = if kind == "video" { "video-review" } else { "audio-review" };
        let child_input = json!({"source_ref": media_ref, "range_ref": range, "question_id": question, "analysis_goal": "text_only"});
        let (_, envelope) = child(ctx, invocation, skill_name, child_input)?;
        media_results.push(json!({"media_ref": media_ref, "version_id": version, "status": status_of(&envelope), "result": data_of(&envelope)}));
    }
    let matter = match skill.as_str() {
        "loan-analysis" => text_of(input, "matter_id")?,
        _ => match versions.first() {
            Some(version) => matter_of_version(ctx, version)?,
            None => return Err(Error::NeedsInput("document_versions")),
        },
    };
    for version in &versions {
        if matter_of_version(ctx, version)? != matter {
            return Err(Error::Denied);
        }
    }
    if skill == "loan-analysis" || skill == "budget-analysis" {
        let (kinds, calc_type, period): (Vec<&str>, &str, Option<(String, String)>) = if skill == "loan-analysis" {
            (vec!["loan_out", "repayment", "principal_received", "payment", "fee", "interest"], "loan", None)
        } else {
            let start = input["period"]["start"].as_str().ok_or(Error::Contract)?.to_owned();
            let end = input["period"]["end"].as_str().ok_or(Error::Contract)?.to_owned();
            (vec![], "budget", Some((start, end)))
        };
        let accounts: Vec<String> = input["account_ids"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        let inputs = confirmed_cashflows(ctx, &matter, &kinds, &accounts, period.as_ref().map(|(a, b)| (a.as_str(), b.as_str())))?;
        if inputs.is_empty() {
            warn(&mut parts, "no_confirmed_cashflows", "没有已确认的金额记录可计算；请先校对并确认相关记录。");
        } else {
            // The user-supplied period becomes explicit assumption objects (not invented).
            let assumptions: Vec<String> = match &period {
                Some((start, end)) => {
                    let vault = ctx.lock()?;
                    let mut ids = vec![];
                    for (key, value) in [("period_start", start), ("period_end", end)] {
                        let id = vault.register("AssumptionRef", "m1", Some(&matter))?;
                        vault.db().execute("INSERT INTO assumptions(id,matter_id,kind,value,created_at) VALUES(?,?,?,?,?)", params![id, matter, key, json!(value).to_string(), now_rfc3339()])?;
                        ids.push(id);
                    }
                    ids
                }
                None => vec![],
            };
            let (_, envelope) = child(ctx, invocation, "finance-calculate", json!({"calculation_type": calc_type, "confirmed_inputs": inputs, "assumption_ids": assumptions}))?;
            calculation = data_of(&envelope);
            if status_of(&envelope) != "completed" {
                parts.warnings.extend(child_warnings(&envelope));
            }
        }
    }
    let mut retrieval = None;
    if skill != "budget-analysis" {
        let scope = create_scope(ctx, &[matter.clone()], &versions)?;
        let (_, envelope) = child(ctx, invocation, "lvjin-retrieval", json!({"question_id": question, "scope_id": scope, "known_constraint_ids": input["known_constraint_ids"].as_array().cloned().unwrap_or_default(), "requested_output": "evidence_only"}))?;
        retrieval = data_of(&envelope);
        parts.evidence_refs = envelope["evidence_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        parts.warnings.extend(child_warnings(&envelope));
    }
    let payload = json!({"workflow": skill, "matter_id": matter, "question_id": question, "document_versions": versions, "media": media_results,
        "calculation": calculation, "retrieval": retrieval, "analysis": null, "analysis_status": "requires_generation"});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ReviewResultRef", &payload)?;
    parts.data_ref = Some(result);
    parts.status = "requires_generation".into();
    warn(&mut parts, "requires_generation", "分析结论需要配置生成接口；已交付本地证据与确定性计算结果。");
    Ok(parts)
}
