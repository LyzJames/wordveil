//! Composite workflows (design 7.1, 9.1). lvjin-retrieval always delivers
//! hybrid retrieval, rerank and exact source reading; with an approved
//! generation grant it adds planning, a drafted answer, sentence-level
//! verification and at most two revisions. Contract/loan/budget analyses
//! deliver their local evidence and deterministic calculations.
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
    let wants_answer = requested == "answer";
    // Generation runs only with an approved, destination-bound grant for this
    // root; otherwise the local path below is the whole result (design 9.1).
    let can_generate = wants_answer && super::answers::generation_available(ctx, invocation)?;
    let mut parts = completed(None);
    let mut process: Vec<Value> = vec![];
    let mut queries = vec![json!({"query_id": "q0", "query": text})];
    let mut plan_info = json!({"fallback": true, "rewrites": [], "missing_facts": []});
    if can_generate {
        let started = std::time::Instant::now();
        match child(ctx, invocation, "query-plan", json!({"question_id": question, "scope_id": scope, "constraint_ids": constraints})) {
            Ok((_, envelope)) if status_of(&envelope) == "completed" => {
                let plan = match data_of(&envelope) {
                    Some(id) => ctx.lock()?.get_result(&id)?.2,
                    None => json!({}),
                };
                for (index, rewrite) in plan["plan"]["rewritten_queries"].as_array().cloned().unwrap_or_default().iter().enumerate() {
                    queries.push(json!({"query_id": format!("q{}", index + 1), "query": rewrite}));
                }
                plan_info = json!({"fallback": false, "rewrites": plan["rewrites"], "missing_facts": plan["missing_facts"]});
                process.push(json!({"step": "plan", "status": "completed", "ms": started.elapsed().as_millis() as u64, "rewrites": queries.len() - 1}));
            }
            Ok((_, envelope)) => {
                parts.warnings.extend(child_warnings(&envelope));
                warn(&mut parts, "plan_fallback", "查询规划未完成，改用原始问题检索。");
                process.push(json!({"step": "plan", "status": status_of(&envelope), "ms": started.elapsed().as_millis() as u64}));
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => {
                warn(&mut parts, "plan_fallback", "查询规划未完成，改用原始问题检索。");
                process.push(json!({"step": "plan", "status": "failed", "code": error.code(), "ms": started.elapsed().as_millis() as u64}));
            }
        }
    }
    // Fixed hybrid retrieval: q0 always first; rewrites share the other half
    // of the weight in the two-level RRF (knowledge-search).
    let started = std::time::Instant::now();
    let (_, search) = child(ctx, invocation, "knowledge-search", json!({"queries": queries, "filter_ids": [], "snapshot_id": snapshot}))?;
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
    let candidates = match &candidate_set {
        Some(id) => ctx.lock()?.get_result(id)?.2["candidates"].as_array().map(|c| c.len()).unwrap_or(0),
        None => 0,
    };
    process.push(json!({"step": "search", "status": status_of(&search), "ms": started.elapsed().as_millis() as u64, "queries": queries.len(), "candidates": candidates}));
    if let Some(set) = &candidate_set {
        let started = std::time::Instant::now();
        let (_, reranked) = child(ctx, invocation, "evidence-rerank", json!({"query_ref": question, "candidate_set_id": set, "facet_ids": []}))?;
        bundle = data_of(&reranked);
        evidence = reranked["evidence_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        if status_of(&reranked) != "completed" {
            parts.status = "partial".into();
            parts.warnings.extend(child_warnings(&reranked));
        }
        process.push(json!({"step": "rerank", "status": status_of(&reranked), "ms": started.elapsed().as_millis() as u64, "evidence": evidence.len()}));
        if !evidence.is_empty() {
            let (_, reading) = child(ctx, invocation, "evidence-read", json!({"evidence_ids": evidence, "context_mode": "exact"}))?;
            read = data_of(&reading);
        }
    }
    let read_payload = match &read {
        Some(id) => ctx.lock()?.get_result(id)?.2,
        None => json!({"items": []}),
    };
    let mut answer = Value::Null;
    if can_generate {
        match &bundle {
            Some(bundle) if !evidence.is_empty() => {
                answer = compose_and_verify(ctx, invocation, &question, bundle, &mut parts, &mut process)?;
            }
            _ => {
                warn(&mut parts, "insufficient_evidence", "依据不足：所选范围内没有检索到可用于回答的原文，未生成回答。");
                parts.status = "partial".into();
            }
        }
    }
    let usage = generation_usage(ctx, invocation.root_id())?;
    let connection = match super::answers::grant_for_root(ctx, invocation.root_id())? {
        Some(grant) if can_generate => {
            let vault = ctx.lock()?;
            let connection = crate::generation::connections::load(&vault, &grant.connection_id)?;
            json!({"name": connection.name, "host": grant.host, "model_id": connection.profile.model_id, "provider": connection.profile.provider.as_str()})
        }
        _ => Value::Null,
    };
    let notes = if can_generate { "已按授权使用生成接口；回答逐句核查" } else if wants_answer { "未连接或未授权生成接口；只交付本地检索到的原文依据" } else { "只检索原文" };
    let payload = json!({"question_id": question, "question": text, "scope": {"matters": matters, "versions": versions}, "known_constraint_ids": constraints,
        "candidate_set": candidate_set, "evidence_bundle": bundle, "evidence_read": read, "items": read_payload["items"], "requested_output": requested,
        "query_rewrites": queries.iter().skip(1).map(|q| q["query"].clone()).collect::<Vec<_>>(), "plan": plan_info, "answer": answer, "process": process,
        "usage": usage, "connection": connection, "notes": notes, "created_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "RetrievalResultRef", &payload)?;
    parts.data_ref = Some(result);
    parts.evidence_refs = evidence;
    if parts.evidence_refs.is_empty() && !parts.warnings.iter().any(|(code, _)| code == "insufficient_evidence") {
        warn(&mut parts, "insufficient_evidence", "依据不足：所选范围内没有检索到相关原文。");
    }
    if wants_answer && !can_generate {
        parts.status = "requires_generation".into();
        warn(&mut parts, "requires_generation", "生成回答需要连接并授权生成接口；已交付本地检索到的原文依据。");
    }
    Ok(parts)
}

/// Draft, grounding and at most two revisions (root limits: 3 answer versions,
/// 7 generation attempts, 36 grounding units). The last verified version is
/// the answer; remaining problems stay visible instead of being hidden.
fn compose_and_verify(ctx: &Ctx, invocation: &Invocation, question: &str, bundle: &str, parts: &mut EnvelopeParts, process: &mut Vec<Value>) -> Result<Value> {
    let mut draft: Option<String> = None;
    let mut report: Option<String> = None;
    let mut previous_problems: Option<Vec<String>> = None;
    let mut revisions = 0;
    for round in 0..3 {
        let step = if round == 0 { "draft" } else { "revise" };
        let started = std::time::Instant::now();
        let input = json!({"question_id": question, "evidence_bundle_id": bundle, "calculation_ids": [], "draft_ref": draft, "validation_report_ref": report});
        let composed = match child(ctx, invocation, "answer-compose", input) {
            Ok((_, envelope)) => envelope,
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => {
                process.push(json!({"step": step, "status": "failed", "code": error.code(), "ms": started.elapsed().as_millis() as u64}));
                if draft.is_none() {
                    return Err(error);
                }
                warn(parts, "revision_stopped", "修订未完成，保留上一版回答。");
                break;
            }
        };
        let Some(next_draft) = data_of(&composed).filter(|_| status_of(&composed) == "completed") else {
            parts.warnings.extend(child_warnings(&composed));
            process.push(json!({"step": step, "status": status_of(&composed), "ms": started.elapsed().as_millis() as u64}));
            if draft.is_none() {
                parts.status = "partial".into();
                return Ok(Value::Null);
            }
            break;
        };
        process.push(json!({"step": step, "status": "completed", "ms": started.elapsed().as_millis() as u64}));
        if round > 0 {
            revisions += 1;
        }
        let started = std::time::Instant::now();
        let (_, grounded) = child(ctx, invocation, "answer-grounding", json!({"question_id": question, "draft_ref": next_draft, "evidence_bundle_id": bundle, "calculation_ids": []}))?;
        let next_report = data_of(&grounded);
        if next_report.is_none() {
            parts.warnings.extend(child_warnings(&grounded));
        }
        let findings = match &next_report {
            Some(id) => ctx.lock()?.get_result(id)?.2,
            None => json!({}),
        };
        let problems: Vec<String> = findings["required_actions"].as_array().cloned().unwrap_or_default().iter()
            .map(|a| format!("{}:{}", a["claim_id"].as_str().unwrap_or(""), a["reason"].as_str().unwrap_or(""))).collect();
        let supported = findings["claims"].as_array().map(|c| c.iter().filter(|r| r["status"] == "supported").count()).unwrap_or(0);
        process.push(json!({"step": "verify", "status": status_of(&grounded), "ms": started.elapsed().as_millis() as u64, "supported": supported, "problems": problems.len()}));
        draft = Some(next_draft);
        report = next_report;
        if problems.is_empty() || round == 2 || previous_problems.as_ref() == Some(&problems) {
            break;
        }
        previous_problems = Some(problems);
    }
    let (Some(draft), Some(report)) = (draft, report) else {
        // An answer that could not be verified is not delivered (design 9.12).
        warn(parts, "verification_failed", "回答未能完成逐句核查，未交付；已保留本机检索到的原文依据。");
        parts.status = "partial".into();
        return Ok(Value::Null);
    };
    match ctx.lock()?.runtime.finalize(invocation) {
        Ok(()) | Err(Error::Denied) => {}
        Err(error) => return Err(error),
    }
    let (draft_payload, report_payload) = {
        let vault = ctx.lock()?;
        (vault.get_result(&draft)?.2, vault.get_result(&report)?.2)
    };
    let labels: Vec<(String, String)> = draft_payload["evidence_labels"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_owned())).collect()).unwrap_or_default();
    let checks: std::collections::BTreeMap<String, Value> = report_payload["claims"].as_array().cloned().unwrap_or_default().into_iter()
        .filter_map(|r| Some((r["claim_id"].as_str()?.to_owned(), r))).collect();
    let mut sections = serde_json::Map::new();
    for (section, kind) in [("conclusion", "conclusion"), ("basis", "basis"), ("calculations", "calculation"), ("next_steps", "next_step")] {
        let rows: Vec<Value> = draft_payload["draft"]["claims"].as_array().cloned().unwrap_or_default().into_iter().filter(|c| c["kind"] == kind).map(|claim| {
            let id = claim["claim_id"].as_str().unwrap_or("").to_owned();
            let check = checks.get(&id).cloned().unwrap_or(Value::Null);
            let cited: Vec<Value> = claim["evidence_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|r| {
                let r = r.as_str()?;
                labels.iter().find(|(_, v)| v == r).map(|(label, _)| json!({"label": label, "evidence_id": r}))
            }).collect();
            json!({"claim_id": id, "text": claim["text"], "evidence": cited, "status": check["status"], "message": check["message"], "flagged": check["flagged"]})
        }).collect();
        sections.insert(section.into(), Value::Array(rows));
    }
    sections.insert("assumptions".into(), draft_payload["draft"]["assumptions"].clone());
    sections.insert("missing_information".into(), draft_payload["draft"]["unknowns"].clone());
    sections.insert("conflicts".into(), draft_payload["draft"]["conflicts"].clone());
    let count = |status: &str| report_payload["claims"].as_array().map(|c| c.iter().filter(|r| r["status"] == status).count()).unwrap_or(0);
    let (unsupported, unchecked) = (count("unsupported"), count("unchecked"));
    if unsupported > 0 {
        warn(parts, "unsupported_claims", "回答中仍有内容未找到依据，已标出，请谨慎使用。");
    }
    if unchecked > 0 || unsupported > 0 || draft_payload["insufficient_evidence"] == json!(true) {
        parts.status = "partial".into();
    }
    if draft_payload["insufficient_evidence"] == json!(true) {
        warn(parts, "insufficient_evidence", "依据不足：已说明缺少哪些信息。");
    }
    Ok(json!({"draft_ref": draft, "validation_report_ref": report, "version": draft_payload["version"], "revisions": revisions,
        "insufficient_evidence": draft_payload["insufficient_evidence"], "sections": Value::Object(sections),
        "labels": labels.iter().map(|(l, r)| json!({"label": l, "evidence_id": r})).collect::<Vec<_>>(),
        "coverage": report_payload["coverage"], "verification_note": report_payload["note"]}))
}

fn generation_usage(ctx: &Ctx, root: &str) -> Result<Value> {
    let vault = ctx.lock()?;
    let (requests, input, output): (i64, Option<i64>, Option<i64>) = vault.db().query_row(
        "SELECT COUNT(*),SUM(input_tokens),SUM(output_tokens) FROM generation_requests WHERE root=? AND purpose<>'capability_probe'",
        [root],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(json!({"requests": requests, "input_tokens": input.unwrap_or(0), "output_tokens": output.unwrap_or(0)}))
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
