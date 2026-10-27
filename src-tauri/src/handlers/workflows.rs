//! Composite workflows (design 7.1, 9.1). lvjin-retrieval always delivers
//! hybrid retrieval, rerank and exact source reading; with an approved
//! generation grant the LangGraph answer flow adds planning, a drafted
//! answer, sentence-level verification and at most two revisions, and the
//! core builds the delivered answer. Contract/loan/budget analyses deliver
//! their local evidence and deterministic calculations.
use super::*;
use crate::vault::now_rfc3339;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// Core-created retrieval scope (matters and optional versions).
pub fn create_scope(ctx: &Ctx, matters: &[String], versions: &[String]) -> Result<String> {
    create_scope_with(ctx,matters,versions,&crate::retrieval_scope::Policy::default())
}
pub fn create_scope_with(ctx: &Ctx, matters: &[String], versions: &[String],policy:&crate::retrieval_scope::Policy) -> Result<String> {
    policy.validate()?;
    let mut policy=policy.clone();policy.as_of=Some(policy.date()?);
    let vault = ctx.lock()?;
    let infer_matters=matters.is_empty();
    let mut matters=matters.to_vec();
    for matter in &matters {
        vault.expect_kind(matter, &["MatterRef", "PublicKnowledgeCollectionRef"])?;
    }
    for version in versions {
        vault.expect_kind(version, &["DocumentVersionRef"])?;
        let owner:String=vault.db().query_row("SELECT d.matter_id FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?",[version],|r|r.get(0))?;
        if infer_matters {if !matters.contains(&owner){matters.push(owner);}}else if !matters.contains(&owner){return Err(Error::Denied);}
    }
    if versions.is_empty() {
        let mut statement=vault.db().prepare("SELECT collection_id FROM knowledge_packages WHERE state='active' ORDER BY package_id")?;
        let public=statement.query_map([],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        for collection in public {if !matters.contains(&collection){matters.push(collection);}}
    }
    let id = vault.register("ScopeRef", "0.2.0", None)?;
    vault.db().execute("INSERT INTO scopes(id,matters,versions,policy,created_at) VALUES(?,?,?,?,?)", params![id, serde_json::to_string(&matters)?, serde_json::to_string(versions)?,serde_json::to_string(&policy)?, now_rfc3339()])?;
    Ok(id)
}

fn scope_contents(ctx: &Ctx, scope: &str) -> Result<(Vec<String>, Vec<String>)> {
    let (matters, versions): (String, String) = ctx.lock()?.db().query_row("SELECT matters,versions FROM scopes WHERE id=?", [scope], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
    Ok((serde_json::from_str(&matters)?, serde_json::from_str(&versions)?))
}

/// What one retrieval produced, whichever path ran.
struct Found {
    queries: Vec<Value>,
    plan: Value,
    candidate_set: Option<String>,
    bundle: Option<String>,
    read: Option<String>,
    evidence: Vec<String>,
    answer: Value,
    process: Vec<Value>,
    run: Option<String>,
    /// Roles, plan and checker summaries of a deep answer (core-built).
    extra: Value,
}

pub fn retrieval(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    super::indexing::ensure_dense(ctx)?;
    let question = text_of(input, "question_id")?;
    let scope = text_of(input, "scope_id")?;
    let constraints = refs_of(input, "known_constraint_ids")?;
    let requested = text_of(input, "requested_output")?;
    let text: String = ctx.lock()?.db().query_row("SELECT text FROM questions WHERE id=?", [&question], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
    let (matters, versions) = scope_contents(ctx, &scope)?;
    let policy:Value=serde_json::from_str(&ctx.lock()?.db().query_row("SELECT policy FROM scopes WHERE id=?",[&scope],|r|r.get::<_,String>(0))?)?;
    let wants_answer = requested == "answer";
    // Generation runs only with an approved, destination-bound grant for this
    // root (design 9.1); the LangGraph answer flow then owns the steps. Without
    // one, the fixed local pipeline below is the whole result.
    let can_generate = wants_answer && super::answers::generation_available(ctx, invocation)?;
    let mut parts = completed(None);
    let snapshot = |ctx: &Ctx| -> Result<String> {
        let generation = super::indexing::current_generation(ctx)?;
        ctx.lock()?.put_result(invocation.skill(), invocation.id(), "RetrievalSnapshotRef", &json!({"generation": generation, "matters": matters, "versions": versions, "scope_id": scope,"policy":policy}))
    };
    let found = if can_generate {
        answer_flow(ctx, invocation, input, &question, &text, &scope, &constraints, &snapshot, &mut parts)?
    } else {
        local_evidence(ctx, invocation, &question, &text, &snapshot(ctx)?, &mut parts)?
    };
    let mut read_payload = match &found.read {
        Some(id) => ctx.lock()?.get_result(id)?.2,
        None => json!({"items": []}),
    };
    let candidate_payload=match &found.candidate_set {Some(id)=>ctx.lock()?.get_result(id)?.2,None=>Value::Null};
    if let Some(items)=read_payload["items"].as_array_mut() {
        for item in items {
            if let Some(candidate)=candidate_payload["candidates"].as_array().and_then(|rows|rows.iter().find(|c|c["evidence_id"]==item["evidence_id"])) {if candidate["graph_paths"].is_array(){item["graph_paths"]=candidate["graph_paths"].clone();}}
        }
    }
    let usage = generation_usage(ctx, invocation.root_id())?;
    let connection = if can_generate { connection_summary(ctx, invocation.root_id())? } else { Value::Null };
    let notes = if can_generate { "已按授权使用生成接口；回答逐句核查" } else if wants_answer { "未连接或未授权生成接口；只交付本地检索到的原文依据" } else { "只检索原文" };
    let payload = json!({"question_id": question, "question": text, "scope": {"matters": matters, "versions": versions}, "known_constraint_ids": constraints,
        "candidate_set": found.candidate_set, "evidence_bundle": found.bundle, "evidence_read": found.read, "items": read_payload["items"], "requested_output": requested,
        "query_rewrites": found.queries.iter().skip(1).map(|q| q["query"].clone()).collect::<Vec<_>>(), "plan": found.plan, "answer": found.answer, "process": found.process,
        "usage": usage, "connection": connection, "notes": notes, "flow_run": found.run, "roles": found.extra["roles"], "task_plan": found.extra["task_plan"],
        "challenge": found.extra["challenge"],"graph":candidate_payload["graph"], "created_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "RetrievalResultRef", &payload)?;
    parts.data_ref = Some(result);
    parts.evidence_refs = found.evidence;
    if parts.evidence_refs.is_empty() && !found.process.iter().any(|p| p["status"] != "completed") && !parts.warnings.iter().any(|(code, _)| code == "insufficient_evidence" || code == "retrieval_failed") {
        warn(&mut parts, "insufficient_evidence", "依据不足：所选范围内没有检索到相关原文。");
    }
    if wants_answer && !can_generate {
        parts.status = "requires_generation".into();
        warn(&mut parts, "requires_generation", "生成回答需要连接并授权生成接口；已交付本地检索到的原文依据。");
    }
    Ok(parts)
}

/// Fixed local pipeline (no model decision): the original question q0 →
/// hybrid search → rerank → exact reading.
fn local_evidence(ctx: &Ctx, invocation: &Invocation, question: &str, text: &str, snapshot: &str, parts: &mut EnvelopeParts) -> Result<Found> {
    let queries = vec![json!({"query_id": "q0", "query": text})];
    let mut process = vec![];
    let started = std::time::Instant::now();
    ctx.report(invocation,"search",0,1);
    let (_, search) = child(ctx, invocation, "knowledge-search", json!({"queries": queries, "filter_ids": [], "snapshot_id": snapshot}))?;
    if status_of(&search) != "completed" {
        parts.warnings.extend(child_warnings(&search));
        if status_of(&search) != "partial" {
            parts.status = "partial".into();
        }
    }
    let mut candidate_set = data_of(&search);
    let candidates = match &candidate_set {
        Some(id) => ctx.lock()?.get_result(id)?.2["candidates"].as_array().map(|c| c.len()).unwrap_or(0),
        None => 0,
    };
    process.push(json!({"step": "search", "status": status_of(&search), "ms": started.elapsed().as_millis() as u64, "queries": 1, "candidates": candidates}));
    if let Some(set)=candidate_set.clone() {
        let enabled=ctx.lock()?.get_result(&set)?.2["graph_enabled"]==json!(true);
        if enabled {
            let started=std::time::Instant::now();
            let (_,expanded)=child(ctx,invocation,"graph-relations",json!({"seed_ids":[],"relation_types":[],"max_hops":2,"candidate_set_id":set,"snapshot_id":snapshot}))?;
            if let Some(result)=data_of(&expanded) {let payload=ctx.lock()?.get_result(&result)?.2;if let Some(set)=payload["candidate_set"].as_str(){candidate_set=Some(set.into());}}
            parts.warnings.extend(child_warnings(&expanded));
            process.push(json!({"step":"graph","status":status_of(&expanded),"ms":started.elapsed().as_millis() as u64}));
        }
    }
    let (mut bundle, mut read, mut evidence) = (None, None, vec![]);
    if let Some(set) = &candidate_set {
        let started = std::time::Instant::now();
        ctx.report(invocation,"rerank",0,1);
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
    Ok(Found { queries, plan: json!({"fallback": true, "rewrites": [], "missing_facts": []}), candidate_set, bundle, read, evidence, answer: Value::Null, process, run: None,
        extra: json!({"roles": [], "task_plan": null, "challenge": null}) })
}

/// The LangGraph answer flow; the core re-checks every reference it returns
/// and builds the delivered answer itself from the verified draft and report.
#[allow(clippy::too_many_arguments)]
fn answer_flow(
    ctx: &Ctx, invocation: &Invocation, input: &Value, question: &str, text: &str, scope: &str, constraints: &[String], snapshot: &dyn Fn(&Ctx) -> Result<String>, parts: &mut EnvelopeParts,
) -> Result<Found> {
    // "深入分析" approves a planner and narrow roles; otherwise the fixed answer flow.
    let deep = super::answers::grant_for_root(ctx, invocation.root_id())?.is_some_and(|g| g.purpose == "deep_answer");
    let outcome = super::flows::run(ctx, invocation, input, if deep { "deep_answer" } else { "answer" }, || {
        Ok(json!({"owner": invocation.id(), "question_id": question, "question": text, "scope_id": scope, "constraint_ids": constraints, "snapshot_id": snapshot(ctx)?, "generate": true}))
    })?;
    let out = &outcome.output;
    let root = invocation.root_id();
    let plan = super::flows::owned_result(ctx, &out["plan_ref"], "SearchPlanRef", root)?;
    let candidate_set = super::flows::owned_result(ctx, &out["candidate_set"], "CandidateSetRef", root)?.map(|(id, _)| id);
    let bundle = super::flows::owned_result(ctx, &out["evidence_bundle"], "EvidenceBundleRef", root)?;
    let read = super::flows::owned_result(ctx, &out["evidence_read"], "EvidenceReadRef", root)?.map(|(id, _)| id);
    let draft = super::flows::owned_result(ctx, &out["draft"], "DraftRef", root)?;
    let report = super::flows::owned_result(ctx, &out["report"], "ValidationReportRef", root)?;
    // Evidence is what the core put into the bundle, not what the flow says.
    let evidence: Vec<String> = match &bundle {
        Some((_, payload)) => payload["items"].as_array().cloned().unwrap_or_default().iter().take(payload["initial_take"].as_u64().unwrap_or(0) as usize)
            .filter_map(|item| item["evidence_id"].as_str().map(str::to_owned)).collect(),
        None => vec![],
    };
    let mut queries = vec![json!({"query_id": "q0", "query": text})];
    let plan_info = match &plan {
        Some((_, payload)) => {
            for (index, rewrite) in payload["plan"]["rewritten_queries"].as_array().cloned().unwrap_or_default().iter().enumerate() {
                queries.push(json!({"query_id": format!("q{}", index + 1), "query": rewrite}));
            }
            json!({"fallback": false, "rewrites": payload["rewrites"], "missing_facts": payload["missing_facts"]})
        }
        None => json!({"fallback": true, "rewrites": [], "missing_facts": []}),
    };
    for (code, message) in outcome.warnings {
        if !parts.warnings.iter().any(|(c, _)| *c == code) {
            parts.warnings.push((code, message));
        }
    }
    for code in out["warnings"].as_array().cloned().unwrap_or_default().iter().filter_map(Value::as_str) {
        if let Some(message) = super::flows::flow_warning(code) {
            if !parts.warnings.iter().any(|(c, _)| c == code) {
                warn(parts, code, &message);
            }
        }
    }
    if out["partial"] == json!(true) {
        parts.status = "partial".into();
    }
    let answer = match (&draft, &report) {
        (Some((draft, _)), Some((report, _))) => deliver_answer(ctx, invocation, draft, report, out["revisions"].as_u64().unwrap_or(0).min(2), parts)?,
        (Some(_), None) => {
            // An answer that could not be verified is not delivered (design 9.12).
            warn(parts, "verification_failed", "回答未能完成逐句核查，未交付；已保留本机检索到的原文依据。");
            parts.status = "partial".into();
            Value::Null
        }
        (None, _) => {
            if bundle.is_some() && !evidence.is_empty() {
                parts.status = "partial".into();
            }
            Value::Null
        }
    };
    let extra = json!({"roles": role_summaries(ctx, root, &out["analyses"])?, "task_plan": plan_summary(ctx, root, &out["task_plan"])?, "challenge": challenge_summary(ctx, root, &out["challenge"])?});
    Ok(Found { queries, plan: plan_info, candidate_set, bundle: bundle.map(|(id, _)| id), read, evidence, answer, process: super::flows::clean_process(&out["process"]), run: Some(outcome.run), extra })
}

/// Finalize and shape the last verified version (root limits: 3 answer
/// versions, 36 grounding units); remaining problems stay visible.
pub(crate) fn deliver_answer(ctx: &Ctx, invocation: &Invocation, draft: &str, report: &str, revisions: u64, parts: &mut EnvelopeParts) -> Result<Value> {
    let (draft_payload, report_payload) = {
        let vault = ctx.lock()?;
        (vault.get_result(draft)?.2, vault.get_result(report)?.2)
    };
    if report_payload["draft_ref"].as_str() != Some(draft) {
        return Err(Error::Denied);
    }
    match ctx.lock()?.runtime.finalize(invocation) {
        Ok(()) | Err(Error::Denied) => {}
        Err(error) => return Err(error),
    }
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
            json!({"claim_id": id, "text": claim["text"], "evidence": cited, "status": check["status"], "message": check["message"], "flagged": check["flagged"], "arithmetic": check["arithmetic"]})
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
        "insufficient_evidence": draft_payload["insufficient_evidence"], "sections": Value::Object(sections), "follow_ups": draft_payload["follow_ups"],
        "labels": labels.iter().map(|(l, r)| json!({"label": l, "evidence_id": r})).collect::<Vec<_>>(),
        "coverage": report_payload["coverage"], "verification_note": report_payload["note"]}))
}

/// The connection a root's grant names (name, host, model), for the result.
pub(crate) fn connection_summary(ctx: &Ctx, root: &str) -> Result<Value> {
    Ok(match super::answers::grant_for_root(ctx, root)? {
        Some(grant) => {
            let vault = ctx.lock()?;
            let connection = crate::generation::connections::load(&vault, &grant.connection_id)?;
            json!({"name": connection.name, "host": grant.host, "model_id": connection.profile.model_id, "provider": connection.profile.provider.as_str()})
        }
        None => Value::Null,
    })
}

pub(crate) fn generation_usage(ctx: &Ctx, root: &str) -> Result<Value> {
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

fn confirmed_cashflows(ctx: &Ctx, matter: &str, versions: &[String], kinds: &[&str], accounts: &[String], period: Option<(&str, &str)>) -> Result<Vec<String>> {
    let vault = ctx.lock()?;
    let mut statement = vault.db().prepare("SELECT c.id,c.kind,c.account_id,c.occurred_on,c.source_version FROM cashflows c JOIN objects o ON o.id=c.id WHERE c.matter_id=? AND c.status='confirmed' AND o.active=1 ORDER BY c.occurred_on,c.id")?;
    let rows = statement.query_map([matter], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?)))?;
    let mut output = vec![];
    for row in rows {
        let (id, kind, account, date, source) = row?;
        if !versions.is_empty() && !source.as_ref().is_some_and(|v| versions.contains(v)) {
            continue;
        }
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
    let can_generate = super::answers::generation_available(ctx, invocation)?;
    let mut parts = completed(None);
    let mut calculation = None;
    let mut media_results = vec![];
    for media_ref in &media {
        // Media become locatable text evidence first (text_only).
        let (kind, version): (String, String) = ctx.lock()?.db().query_row("SELECT v.media_kind,v.id FROM media m JOIN document_versions v ON v.id=m.version_id WHERE m.id=?", [media_ref], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
        let range = {
            let vault = ctx.lock()?;
            let id = vault.register("ApprovedTimeRangeRef", "0.2.0", Some(media_ref))?;
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
    if can_generate && (skill == "loan-analysis" || skill == "budget-analysis") {
        // A written analysis reuses only the immutable calculation that was
        // shown and approved. It never silently recomputes a wider set.
        let grant = super::answers::grant_for_root(ctx, invocation.root_id())?.ok_or(Error::Denied)?;
        calculation = grant.calculations.first().and_then(|v| v["calculation_id"].as_str()).map(str::to_owned);
        if calculation.is_none() {
            if grant.excluded.is_empty() {
                warn(&mut parts, "no_confirmed_cashflows", "所选资料中没有已确认的金额记录可计算；书面分析仅使用已授权原文。");
            } else {
                warn(&mut parts, "calculation_not_approved", "本次未授权发送计算结果，书面分析仅使用已授权原文。");
            }
        }
    } else if skill == "loan-analysis" || skill == "budget-analysis" {
        let (kinds, calc_type, period): (Vec<&str>, &str, Option<(String, String)>) = if skill == "loan-analysis" {
            (vec!["loan_out", "repayment", "principal_received", "payment", "fee", "interest"], "loan", None)
        } else {
            let start = input["period"]["start"].as_str().ok_or(Error::Contract)?.to_owned();
            let end = input["period"]["end"].as_str().ok_or(Error::Contract)?.to_owned();
            (vec![], "budget", Some((start, end)))
        };
        let accounts: Vec<String> = input["account_ids"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        let inputs = confirmed_cashflows(ctx, &matter, &versions, &kinds, &accounts, period.as_ref().map(|(a, b)| (a.as_str(), b.as_str())))?;
        if inputs.is_empty() {
            warn(&mut parts, "no_confirmed_cashflows", "没有已确认的金额记录可计算；请先校对并确认相关记录。");
        } else {
            // The user-supplied period becomes explicit assumption objects (not invented).
            let assumptions: Vec<String> = match &period {
                Some((start, end)) => {
                    let vault = ctx.lock()?;
                    let mut ids = vec![];
                    for (key, value) in [("period_start", start), ("period_end", end)] {
                        let id = vault.register("AssumptionRef", "0.2.0", Some(&matter))?;
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
    let mut scope = None;
    // Budget analysis reads its bills only when a written analysis follows.
    if skill != "budget-analysis" || can_generate {
        let created = create_scope(ctx, &[matter.clone()], &versions)?;
        let (_, envelope) = child(ctx, invocation, "lvjin-retrieval", json!({"question_id": question, "scope_id": created, "known_constraint_ids": input["known_constraint_ids"].as_array().cloned().unwrap_or_default(), "requested_output": "evidence_only"}))?;
        retrieval = data_of(&envelope);
        parts.evidence_refs = envelope["evidence_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        parts.warnings.extend(child_warnings(&envelope).into_iter().filter(|(code, _)| code != "requires_generation"));
        scope = Some(created);
    }
    let mut written = json!({"analysis": null, "analysis_status": "requires_generation", "roles": [], "process": []});
    if can_generate {
        written = written_analysis(ctx, invocation, input, &skill, &question, scope.as_deref(), retrieval.as_deref(), calculation.as_deref(), &mut parts)?;
    }
    let mut payload = json!({"workflow": skill, "matter_id": matter, "question_id": question, "document_versions": versions, "media": media_results,
        "calculation": calculation, "retrieval": retrieval});
    for (key, value) in written.as_object().cloned().unwrap_or_default() {
        payload[key] = value;
    }
    payload["created_at"] = json!(now_rfc3339());
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ReviewResultRef", &payload)?;
    parts.data_ref = Some(result);
    if !can_generate {
        parts.status = "requires_generation".into();
        warn(&mut parts, "requires_generation", "分析结论需要配置并授权生成接口；已交付本地证据与确定性计算结果。");
    }
    Ok(parts)
}

/// The LangGraph analysis flow: it opens the finalizing lvjin-retrieval child,
/// runs the fixed roles under it and closes it; the core re-checks every
/// reference and builds what the user sees.
#[allow(clippy::too_many_arguments)]
fn written_analysis(
    ctx: &Ctx, invocation: &Invocation, input: &Value, skill: &str, question: &str, scope: Option<&str>, retrieval: Option<&str>, calculation: Option<&str>, parts: &mut EnvelopeParts,
) -> Result<Value> {
    let root = invocation.root_id();
    let flow = skill.replace('-', "_");
    let (scope, retrieval) = match (scope, retrieval) {
        (Some(scope), Some(retrieval)) => (scope, retrieval),
        _ => return Err(Error::Integrity),
    };
    let retrieved = ctx.lock()?.get_result(retrieval)?.2;
    let bundle = retrieved["evidence_bundle"].clone();
    if bundle.is_null() {
        if retrieved["process"].as_array().is_some_and(|steps| steps.iter().any(|p| matches!(p["step"].as_str(), Some("search" | "rerank")) && p["status"] != "completed")) {
            warn(parts, "retrieval_failed", "本机检索未完成，未生成书面分析；请查看具体失败原因后重新准备。");
            parts.status = "partial".into();
            return Ok(json!({"analysis": null, "analysis_status": "retrieval_failed", "roles": [], "process": retrieved["process"]}));
        }
        warn(parts, "insufficient_evidence", "依据不足：所选资料中没有检索到可用于分析的原文，未生成书面分析。");
        parts.status = "partial".into();
        return Ok(json!({"analysis": null, "analysis_status": "insufficient_evidence", "roles": [], "process": []}));
    }
    let text: String = ctx.lock()?.db().query_row("SELECT text FROM questions WHERE id=?", [question], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
    let outcome = super::flows::run(ctx, invocation, input, &flow, || {
        Ok(json!({"owner": invocation.id(), "question_id": question, "question": text, "evidence_bundle": bundle, "calculation_ids": calculation.map(|c| vec![c]).unwrap_or_default(),
            "retrieval_input": {"question_id": question, "scope_id": scope, "known_constraint_ids": input["known_constraint_ids"].as_array().cloned().unwrap_or_default(), "requested_output": "answer"}}))
    })?;
    let out = &outcome.output;
    for (code, message) in outcome.warnings {
        if !parts.warnings.iter().any(|(c, _)| *c == code) {
            parts.warnings.push((code, message));
        }
    }
    let analysis = super::flows::owned_result(ctx, &out["retrieval"], "RetrievalResultRef", root)?;
    let (analysis_id, answer) = match &analysis {
        Some((id, payload)) => (json!(id), payload["answer"].clone()),
        None => (Value::Null, Value::Null),
    };
    if answer.is_null() {
        parts.status = "partial".into();
    } else if answer["sections"]["conclusion"].as_array().is_some_and(|c| c.iter().any(|row| row["status"] != "supported")) {
        parts.status = "partial".into();
    }
    Ok(json!({"analysis": analysis_id, "answer": answer, "analysis_status": if answer.is_null() { "partial" } else { "completed" },
        "roles": role_summaries(ctx, root, &out["analyses"])?, "challenge": challenge_summary(ctx, root, &out["challenge"])?,
        "process": super::flows::clean_process(&out["process"]), "flow_run": outcome.run}))
}

/// The validated task plan with its goals as the user reads them.
pub(crate) fn plan_summary(ctx: &Ctx, root: &str, reference: &Value) -> Result<Value> {
    let Some((id, payload)) = super::flows::owned_result(ctx, reference, "TaskPlanRef", root)? else { return Ok(Value::Null) };
    let vault = ctx.lock()?;
    let text = |id: &Value| -> Result<Value> {
        Ok(match id.as_str() {
            Some(id) => json!(vault.db().query_row::<String, _, _>("SELECT text FROM goals WHERE id=? AND root=?", params![id, root], |r| r.get(0)).optional()?),
            None => Value::Null,
        })
    };
    let mut tasks = vec![];
    for node in payload["plan"]["task_graph"].as_array().cloned().unwrap_or_default() {
        // The planner numbered its passages E1, E2…; every task packet keeps
        // that order, so the interface can show those labels as citations.
        let packet: Option<String> = match node["context_packet_ref"].as_str() {
            Some(id) => vault.db().query_row("SELECT payload FROM context_packets WHERE id=? AND root=?", params![id, root], |r| r.get(0)).optional()?,
            None => None,
        };
        let evidence = packet.and_then(|p| serde_json::from_str::<Value>(&p).ok()).map(|p| p["evidence_ids"].clone()).unwrap_or(Value::Null);
        let labels: serde_json::Map<String, Value> = evidence.as_array().cloned().unwrap_or_default().into_iter().enumerate().map(|(index, id)| (format!("E{}", index + 1), id)).collect();
        tasks.push(json!({"node_id": node["node_id"], "role_id": node["role_id"], "depends_on": node["depends_on"], "goal": text(&node["goal_ref"])?, "acceptance": text(&node["acceptance_ref"])?,
            "labels": labels}));
    }
    Ok(json!({"task_plan_ref": id, "tasks": tasks}))
}

/// What each role contributed, built by the core from validated results.
pub(crate) fn role_summaries(ctx: &Ctx, root: &str, refs: &Value) -> Result<Vec<Value>> {
    let mut out = vec![];
    for id in refs.as_array().cloned().unwrap_or_default().into_iter().take(6) {
        if let Some((id, payload)) = super::flows::owned_result(ctx, &id, "RoleAnalysisRef", root)? {
            let labels: Vec<(String, String)> = payload["evidence_labels"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_owned())).collect()).unwrap_or_default();
            let claims: Vec<Value> = payload["analysis"]["claims"].as_array().cloned().unwrap_or_default().iter().map(|claim| {
                let cited: Vec<Value> = claim["evidence_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|r| {
                    let r = r.as_str()?;
                    labels.iter().find(|(_, v)| v == r).map(|(label, _)| json!({"label": label, "evidence_id": r}))
                }).collect();
                json!({"kind": claim["kind"], "text": claim["text"], "evidence": cited})
            }).collect();
            out.push(json!({"analysis_ref": id, "role": payload["role"], "role_title": payload["role_title"], "claims": claims,
                "limitations": payload["analysis"]["limitations"], "proposed_skill_ids": payload["analysis"]["proposed_skill_ids"], "invalid_citations": payload["invalid_citations"]}));
        }
    }
    Ok(out)
}

pub(crate) fn challenge_summary(ctx: &Ctx, root: &str, reference: &Value) -> Result<Value> {
    Ok(match super::flows::owned_result(ctx, reference, "ChallengeReportRef", root)? {
        Some((id, payload)) => json!({"challenge_ref": id, "draft_ref": payload["report"]["draft_ref"], "challenges": payload["report"]["challenges"]}),
        None => Value::Null,
    })
}

/// Seal an lvjin-retrieval owner a flow opened (an analysis's finalizer):
/// re-check the references the flow reports, deliver the verified answer and
/// build the result and envelope in the core.
pub(crate) fn close_retrieval(ctx: &Ctx, invocation: &Invocation, input: &Value, output: &Value, run: &str) -> Result<Value> {
    let root = invocation.root_id();
    let question = text_of(input, "question_id")?;
    let scope = text_of(input, "scope_id")?;
    let text: String = ctx.lock()?.db().query_row("SELECT text FROM questions WHERE id=?", [&question], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
    let (matters, versions) = scope_contents(ctx, &scope)?;
    let bundle = super::flows::owned_result(ctx, &output["evidence_bundle"], "EvidenceBundleRef", root)?;
    let draft = super::flows::owned_result(ctx, &output["draft"], "DraftRef", root)?;
    let report = super::flows::owned_result(ctx, &output["report"], "ValidationReportRef", root)?;
    let mut parts = completed(None);
    for code in output["warnings"].as_array().cloned().unwrap_or_default().iter().filter_map(Value::as_str) {
        if let Some(message) = super::flows::flow_warning(code) {
            if !parts.warnings.iter().any(|(c, _)| c == code) {
                warn(&mut parts, code, &message);
            }
        }
    }
    if output["partial"] == json!(true) {
        parts.status = "partial".into();
    }
    let answer = match (&draft, &report) {
        (Some((draft, _)), Some((report, _))) => deliver_answer(ctx, invocation, draft, report, output["revisions"].as_u64().unwrap_or(0).min(2), &mut parts)?,
        (Some(_), None) => {
            warn(&mut parts, "verification_failed", "回答未能完成逐句核查，未交付；已保留本机检索到的原文依据。");
            parts.status = "partial".into();
            Value::Null
        }
        (None, _) => {
            parts.status = "partial".into();
            Value::Null
        }
    };
    let evidence = match &bundle {
        Some((id, _)) => super::answers::bundle_ids(ctx, id)?,
        None => vec![],
    };
    let usage = generation_usage(ctx, root)?;
    let payload = json!({"question_id": question, "question": text, "scope": {"matters": matters, "versions": versions}, "known_constraint_ids": input["known_constraint_ids"],
        "candidate_set": null, "evidence_bundle": bundle.as_ref().map(|(id, _)| id.clone()), "evidence_read": null, "items": [], "requested_output": "answer",
        "query_rewrites": [], "plan": {"fallback": true, "rewrites": [], "missing_facts": []}, "answer": answer, "process": super::flows::clean_process(&output["process"]),
        "roles": role_summaries(ctx, root, &output["analyses"])?, "challenge": challenge_summary(ctx, root, &output["challenge"])?,
        "usage": usage, "connection": connection_summary(ctx, root)?, "notes": "书面分析：固定角色在授权范围内分析，最终回答逐句核查", "flow_run": run, "created_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "RetrievalResultRef", &payload)?;
    parts.data_ref = Some(result);
    parts.evidence_refs = evidence;
    ctx.lock()?.runtime.envelope(invocation, &parts)
}
