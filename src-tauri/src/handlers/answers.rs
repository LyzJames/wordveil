//! Generation-backed steps (design 7.1-7.4, 8, 9.1, 9.12): query-plan
//! (search_plan, task_plan), answer-compose (draft_answer, role_analysis,
//! role_check, context_brief) and answer-grounding. Native media observation
//! (media-observe / direct_media) is in `observe`.
//!
//! Every request to the user's generation connection goes through `send`:
//! the root must hold `generation.request` and an approved egress grant that
//! names the connection, its host, the question and the material scope; no
//! more distinct passages than the grant allows leave in the whole task; the
//! ledger reserves the attempt (root limit 7) before any byte leaves; the
//! request is audited with counts, material IDs and the locked prompt's hash
//! only. Model output is data: it is parsed against a locked wire schema,
//! labels are mapped back to real references by the core, and the normalised
//! result is validated against the compiled purpose contract.
use super::*;
use crate::{
    generation::{GenRequest, GenResponse, Message, OutputMode, Part, adapters, connections, credentials::CredentialStore, probe},
    vault::now_rfc3339,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Grant {
    pub id: String,
    pub connection_id: String,
    pub host: String,
    pub scope_id: String,
    pub question_id: String,
    pub max_items: i64,
    pub max_requests: i64,
    /// Excerpts the user removed in the preview; never sent in this task.
    pub excluded: Vec<String>,
    /// Native media the user approved for upload in this task.
    pub media: Vec<String>,
    /// What the user approved: answer, deep_answer, an analysis or a media review.
    pub purpose: String,
    /// Bounded media the core may prepare during the run (audio/video review).
    pub policy: Value,
}

pub(crate) fn grant_for_root(ctx: &Ctx, root: &str) -> Result<Option<Grant>> {
    let vault = ctx.lock()?;
    let row: Option<(String, String, String, String, String, i64, i64, String, String)> = vault
        .db()
        .query_row(
            "SELECT id,connection_id,host,scope_id,question_id,max_items,max_requests,preview,purpose FROM egress_grants WHERE root=? AND approved_at IS NOT NULL AND revoked_at IS NULL",
            [root],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
        )
        .optional()?;
    let Some((id, connection_id, host, scope_id, question_id, max_items, max_requests, preview, purpose)) = row else { return Ok(None) };
    let preview: Value = serde_json::from_str(&preview).unwrap_or(Value::Null);
    let strings = |value: &Value| value.as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect::<Vec<_>>();
    let excluded_media = strings(&preview["excluded_media"]);
    let mut statement = vault.db().prepare("SELECT id FROM native_media WHERE grant_id=? ORDER BY created_at,rowid")?;
    let media = statement.query_map([&id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().filter(|m| !excluded_media.contains(m)).collect();
    Ok(Some(Grant { id, connection_id, host, scope_id, question_id, max_items, max_requests, excluded: strings(&preview["excluded"]), media, purpose, policy: preview["media_policy"].clone() }))
}

/// True when this root may use generation (capability and approved grant).
pub(crate) fn generation_available(ctx: &Ctx, invocation: &Invocation) -> Result<bool> {
    let capable = ctx.lock()?.runtime.authorize(invocation, "generation.request").is_ok();
    Ok(capable && grant_for_root(ctx, invocation.root_id())?.is_some())
}

/// What one request carries, for the checks and the audit record.
pub(crate) struct Call {
    pub purpose: &'static str,
    /// Locked prompt ids joined with "+"; their combined hash is recorded.
    pub prompt: String,
    pub prompt_sha: String,
    pub question: String,
    /// Chunk IDs whose text is in the prompt.
    pub evidence: Vec<String>,
    /// NativeMediaRefs whose bytes are in the request.
    pub media: Vec<String>,
}

/// System text assembled from locked prompts, with the combined hash.
pub(crate) fn system_prompt(ctx: &Ctx, ids: &[&str]) -> Result<(String, String, String)> {
    let vault = ctx.lock()?;
    let mut text = vec![];
    let mut digest = Sha256::new();
    for id in ids {
        let (body, sha) = vault.runtime.registry.prompt(id)?;
        text.push(body.trim().to_owned());
        digest.update(sha.as_bytes());
    }
    Ok((text.join("\n\n"), ids.join("+"), format!("{:x}", digest.finalize())))
}

/// Distinct passages already sent to the generation service in this root.
fn sent_evidence(vault: &crate::store::Vault, root: &str) -> Result<BTreeSet<String>> {
    let mut statement = vault.db().prepare("SELECT materials FROM generation_requests WHERE root=? AND purpose<>'capability_probe'")?;
    let rows = statement.query_map([root], |r| r.get::<_, String>(0))?;
    let mut sent = BTreeSet::new();
    for row in rows {
        let materials: Value = serde_json::from_str(&row?).unwrap_or(Value::Null);
        for id in materials["evidence_ids"].as_array().cloned().unwrap_or_default() {
            if let Some(id) = id.as_str() {
                sent.insert(id.to_owned());
            }
        }
    }
    Ok(sent)
}

/// One real request, after every check.
fn send(ctx: &Ctx, invocation: &Invocation, call: &Call, request: &GenRequest) -> Result<(String, GenResponse)> {
    ctx.check()?;
    let grant = grant_for_root(ctx, invocation.root_id())?.ok_or(Error::Denied)?;
    if grant.question_id != call.question {
        return Err(Error::Denied);
    }
    if call.evidence.len() as i64 > grant.max_items {
        return Err(Error::Budget);
    }
    if call.evidence.iter().any(|id| grant.excluded.contains(id)) || call.media.iter().any(|id| !grant.media.contains(id)) {
        return Err(Error::Denied);
    }
    // At most two requests in flight across the application; wait before
    // reserving the attempt so a cancelled wait costs nothing.
    let _slot = ctx.services.generation_slots.acquire(ctx.cancel)?;
    ctx.check()?;
    let request_id = format!("gen_{}", uuid::Uuid::new_v4().simple());
    let (profile, credential, audit, vault_id) = {
        let mut vault = ctx.lock()?;
        let connection = connections::load(&vault, &grant.connection_id)?;
        if connection.status != "active" || connection.profile.endpoint.display_host() != grant.host {
            return Err(Error::Denied);
        }
        let (matters, versions): (String, String) = vault.db().query_row("SELECT matters,versions FROM scopes WHERE id=?", [&grant.scope_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
        let matters: Vec<String> = serde_json::from_str(&matters)?;
        let versions: Vec<String> = serde_json::from_str(&versions)?;
        for id in &call.evidence {
            let (matter, version): (String, String) = vault.db().query_row("SELECT matter_id,version_id FROM chunks WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
            if !matters.contains(&matter) && !versions.contains(&version) {
                return Err(Error::Denied);
            }
        }
        // The grant's passage limit holds for the whole task, not per request.
        let mut sent = sent_evidence(&vault, invocation.root_id())?;
        sent.extend(call.evidence.iter().cloned());
        if sent.len() as i64 > grant.max_items {
            return Err(Error::Budget);
        }
        let used: i64 = vault.db().query_row("SELECT COUNT(*) FROM generation_requests WHERE root=?", [invocation.root_id()], |r| r.get(0))?;
        if used >= grant.max_requests {
            return Err(Error::Budget);
        }
        // Reserve the attempt in the root ledger before anything leaves.
        vault.runtime.dispatch_generation(invocation, &request_id, !call.media.is_empty())?;
        let materials = json!({"question_id": call.question, "evidence_ids": call.evidence, "media_ids": call.media,
            "prompt": format!("{}@{}", call.prompt, &call.prompt_sha[..12])});
        let audit = connections::begin_request(&vault, invocation.root_id(), invocation.id(), &connection, call.purpose, &materials)?;
        (connection.profile, connection.credential, audit, vault.id.clone())
    };
    let started = std::time::Instant::now();
    let outcome = (|| {
        let key = match &credential {
            Some(id) => Some(CredentialStore::for_vault(&ctx.services.layout.data_root, &vault_id)?.get(id)?),
            None if profile.endpoint.is_loopback() => None,
            None => return Err(Error::NeedsInput("api_key")),
        };
        let mut progress = |_: &str| {};
        adapters::call(&profile, key.as_ref(), request, ctx.cancel, &mut progress)
    })();
    let vault = ctx.lock()?;
    match &outcome {
        Ok(response) => connections::finish_request(&vault, &audit, "completed", None, response.usage.input_tokens, response.usage.output_tokens)?,
        Err(error) => connections::finish_request(&vault, &audit, "failed", Some(&error.code()), 0, 0)?,
    }
    ctx.services.log.event(
        "generation.request",
        &[
            ("purpose", crate::logging::Field::Code(call.purpose)),
            ("status", crate::logging::Field::Status(match &outcome { Ok(_) => "completed".into(), Err(e) => e.code() })),
            ("ms", crate::logging::Field::Millis(started.elapsed().as_millis() as u64)),
            ("evidence", crate::logging::Field::Count(call.evidence.len() as i64)),
            ("media", crate::logging::Field::Count(call.media.len() as i64)),
        ],
    );
    outcome.map(|response| (request_id, response))
}

fn output_mode(ctx: &Ctx, invocation: &Invocation, name: &str, schema: &Value) -> Result<(OutputMode, bool)> {
    let grant = grant_for_root(ctx, invocation.root_id())?.ok_or(Error::Denied)?;
    let vault = ctx.lock()?;
    let connection = connections::load(&vault, &grant.connection_id)?;
    let streaming = connections::capability(&vault, &connection, "streaming")? == "supported";
    if connections::capability(&vault, &connection, "strict_schema")? == "supported" {
        return Ok((OutputMode::JsonSchema { name: name.into(), schema: schema.clone() }, streaming));
    }
    if connections::capability(&vault, &connection, "json_object")? == "supported" {
        return Ok((OutputMode::JsonObject, streaming));
    }
    Ok((OutputMode::Text, streaming))
}

fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    serde_json::from_str(&trimmed[start..=end]).ok()
}

/// Generate a JSON object matching the purpose's locked wire schema; one
/// structural repair at most (it counts toward the root's seven attempts).
/// Native media requests get no repair: it would upload the media again.
pub(crate) fn generate_json(ctx: &Ctx, invocation: &Invocation, call: &Call, system: &str, user: &str, media: Vec<Part>, max_tokens: u32) -> Result<(Value, String, GenResponse)> {
    let schema = ctx.lock()?.runtime.registry.wire(call.purpose)?.clone();
    let (mode, streaming) = output_mode(ctx, invocation, call.purpose, &schema)?;
    let validator = jsonschema::validator_for(&schema).map_err(|_| Error::Integrity)?;
    let schema_note = if matches!(mode, OutputMode::JsonSchema { .. }) { String::new() } else { format!("\n\n只输出一个 JSON 对象，结构必须符合下面的 JSON Schema，不要输出其他文字：\n{schema}") };
    let mut first = vec![Part::Text(format!("{user}{schema_note}"))];
    first.extend(media);
    let attempts = if call.media.is_empty() { 2 } else { 1 };
    let mut messages = vec![Message { role: "user", parts: first }];
    for attempt in 0..attempts {
        let request = GenRequest { system: system.into(), messages: messages.clone(), max_output_tokens: max_tokens, output: mode.clone(), tools: vec![], stream: streaming };
        let (request_id, response) = send(ctx, invocation, call, &request)?;
        if response.finish == "length" {
            return Err(Error::Remote("output_truncated".into()));
        }
        match extract_json(&response.text) {
            Some(value) if validator.is_valid(&value) => return Ok((value, request_id, response)),
            _ if attempt + 1 < attempts => {
                messages.push(Message { role: "assistant", parts: vec![Part::Text(response.text.chars().take(8000).collect())] });
                messages.push(Message { role: "user", parts: vec![Part::Text(format!("上面的输出不是符合要求的 JSON。请只输出一个符合下面 Schema 的 JSON 对象，不要其他文字：\n{schema}"))] });
            }
            _ => {}
        }
    }
    Err(Error::Remote("invalid_structured_output".into()))
}

fn usage(response: &GenResponse) -> Value {
    json!({"input_tokens": response.usage.input_tokens, "output_tokens": response.usage.output_tokens})
}

fn question_text(ctx: &Ctx, question: &str) -> Result<String> {
    ctx.lock()?.db().query_row("SELECT text FROM questions WHERE id=?", [question], |r| r.get(0)).optional()?.ok_or(Error::Denied)
}

fn requires_generation(message: &str) -> EnvelopeParts {
    let mut parts = EnvelopeParts { status: "requires_generation".into(), ..Default::default() };
    warn(&mut parts, "requires_generation", message);
    parts
}

// ------------------------------------------------------------ labels in text

/// A bracketed group that holds only evidence labels, e.g. （E1、E3） or [C2].
fn label_group(inner: &str) -> bool {
    let cleaned = inner.trim().trim_start_matches("证据").trim_start_matches("见").trim();
    let tokens: Vec<&str> = cleaned.split(|c: char| matches!(c, '、' | ',' | '，' | ';' | '；' | ' ' | '/')).filter(|t| !t.is_empty()).collect();
    !tokens.is_empty()
        && tokens.iter().all(|token| {
            let mut chars = token.chars();
            matches!(chars.next(), Some('E' | 'C' | 'M' | 'e' | 'c' | 'm')) && (2..=4).contains(&token.len()) && chars.all(|c| c.is_ascii_digit())
        })
}

/// Labels belong in the evidence field; the core removes them from the text
/// (design rule for drafts) and counts how many it removed.
pub fn strip_labels(text: &str) -> (String, usize) {
    const PAIRS: [(char, char); 4] = [('(', ')'), ('（', '）'), ('[', ']'), ('【', '】')];
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut removed = 0;
    let mut index = 0;
    while index < chars.len() {
        if let Some((_, close)) = PAIRS.iter().find(|(open, _)| *open == chars[index]) {
            if let Some(end) = chars[index + 1..].iter().position(|c| c == close).map(|p| index + 1 + p) {
                let inner: String = chars[index + 1..end].iter().collect();
                if label_group(&inner) {
                    removed += 1;
                    index = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    (out.trim().to_owned(), removed)
}

// ------------------------------------------------------------ evidence text

pub(crate) struct EvidenceText {
    pub label: String,
    pub evidence_id: String,
    pub display: String,
    pub location: String,
    pub text: String,
}

/// Active chunks in the given order, labelled E1, E2…
pub(crate) fn evidence_texts(ctx: &Ctx, ids: &[String]) -> Result<Vec<EvidenceText>> {
    let vault = ctx.lock()?;
    let mut out = vec![];
    for (index, id) in ids.iter().enumerate() {
        let (body, version, unit_title, spans): (String, String, String, String) =
            vault.db().query_row("SELECT c.body,c.version_id,c.unit_title,c.spans FROM chunks c JOIN objects o ON o.id=c.id WHERE c.id=? AND o.active=1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?.ok_or(Error::Denied)?;
        let display: String = vault.db().query_row("SELECT d.display_name FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?", [&version], |r| r.get(0))?;
        let block = serde_json::from_str::<Vec<Value>>(&spans)?.iter().find_map(|s| s["block"].as_i64());
        let page: Option<i64> = match block {
            Some(block) => vault.db().query_row("SELECT p.page_no FROM blocks b JOIN pages p ON p.id=b.page_id WHERE b.rowid=?", [block], |r| r.get(0)).optional()?,
            None => None,
        };
        let location = match page {
            Some(page) if page < 100_000 => format!("第{page}页"),
            Some(_) => "视频画面".into(),
            None => String::new(),
        };
        let location = if unit_title.is_empty() { location } else if location.is_empty() { unit_title } else { format!("{location} · {unit_title}") };
        out.push(EvidenceText { label: format!("E{}", index + 1), evidence_id: id.clone(), display, location, text: body });
    }
    Ok(out)
}

/// The passages the core put into a bundle (its initial take), in order.
pub(crate) fn bundle_ids(ctx: &Ctx, bundle: &str) -> Result<Vec<String>> {
    let (_, kind, payload) = ctx.lock()?.get_result(bundle)?;
    if kind != "EvidenceBundleRef" {
        return Err(Error::Denied);
    }
    let take = payload["initial_take"].as_u64().unwrap_or(0) as usize;
    Ok(payload["items"].as_array().cloned().unwrap_or_default().into_iter().take(take).filter_map(|item| item["evidence_id"].as_str().map(str::to_owned)).collect())
}

fn evidence_block(items: &[EvidenceText], max_chars: usize) -> String {
    if items.is_empty() {
        return "（没有给出原文证据）\n".into();
    }
    items.iter().map(|item| format!("[{}] 《{}》{}：{}\n", item.label, item.display, if item.location.is_empty() { String::new() } else { format!(" {}", item.location) }, item.text.chars().take(max_chars).collect::<String>())).collect()
}

fn calculation_text(ctx: &Ctx, ids: &[String]) -> Result<Vec<(String, String)>> {
    let vault = ctx.lock()?;
    let mut out = vec![];
    for (index, id) in ids.iter().enumerate() {
        let (_, kind, payload) = vault.get_result(id)?;
        if kind != "CalculationRef" {
            return Err(Error::Denied);
        }
        out.push((format!("C{}", index + 1), serde_json::to_string(&payload["result"]).unwrap_or_default().chars().take(3000).collect()));
    }
    Ok(out)
}

fn calculation_block(calc: &[(String, String)]) -> String {
    if calc.is_empty() {
        return "无\n".into();
    }
    calc.iter().map(|(label, value)| format!("[{label}] {value}\n")).collect()
}

/// Map wire claims (labels) to claims with references; unknown labels are
/// recorded, never invented. Returns (claims, invalid citations, labels removed from text).
fn map_claims(rows: &[(String, Value)], labels: &BTreeMap<String, String>, calc_labels: &BTreeMap<String, String>, first: usize) -> (Vec<Value>, Vec<Value>, usize) {
    let mut claims = vec![];
    let mut invalid = vec![];
    let mut removed = 0;
    for (kind, row) in rows {
        let claim_id = format!("c{}", first + claims.len());
        let mut evidence_refs: Vec<String> = vec![];
        let mut calculation_refs: Vec<String> = vec![];
        for label in row["evidence"].as_array().cloned().unwrap_or_default() {
            let label = label.as_str().unwrap_or("").trim().to_ascii_uppercase();
            if let Some(id) = labels.get(&label) {
                if !evidence_refs.contains(id) {
                    evidence_refs.push(id.clone());
                }
            } else if let Some(id) = calc_labels.get(&label) {
                if !calculation_refs.contains(id) {
                    calculation_refs.push(id.clone());
                }
            } else {
                invalid.push(json!({"claim_id": claim_id, "label": label}));
            }
        }
        let (text, count) = strip_labels(row["text"].as_str().unwrap_or(""));
        removed += count;
        claims.push(json!({"claim_id": claim_id, "kind": kind, "text": if text.is_empty() { row["text"].clone() } else { json!(text) }, "evidence_refs": evidence_refs, "calculation_refs": calculation_refs}));
    }
    (claims, invalid, removed)
}

// ------------------------------------------------------------------ query-plan

pub fn query_plan(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    match invocation.purpose() {
        Some("task_plan") => task_plan(ctx, invocation, input),
        _ => search_plan(ctx, invocation, input),
    }
}

fn search_plan(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let question = text_of(input, "question_id")?;
    let scope = text_of(input, "scope_id")?;
    let constraints = refs_of(input, "constraint_ids")?;
    if !generation_available(ctx, invocation)? {
        return Ok(requires_generation("查询改写需要已授权的生成接口；将只使用原始问题检索。"));
    }
    let text = question_text(ctx, &question)?;
    let matters = {
        let vault = ctx.lock()?;
        let matters: String = vault.db().query_row("SELECT matters FROM scopes WHERE id=?", [&scope], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
        serde_json::from_str::<Vec<String>>(&matters)?.len()
    };
    let user = format!(
        "原问题：{text}\n已知约束：{}\n检索范围：用户选定的私密事务 {matters} 件（原文为合同、借条、账单、聊天截图或录音转写等）。\n\n请补充 0–3 条有独立目的的检索改写或子问题（原问题由系统保留为 q0，不要重复它）。优先覆盖：材料原文里可能使用的说法、适用地区或事实时间、规则依据、定义/例外/相反证据、需要计算的口径。影响结论但资料里可能没有的事实列入 missing_facts。\n返回 JSON：{{\"rewrites\": [{{\"purpose\": \"简短目的\", \"query\": \"检索词或子问题\"}}], \"missing_facts\": [\"…\"]}}",
        if constraints.is_empty() { "无".to_owned() } else { format!("{} 条已确认约束", constraints.len()) }
    );
    let (system, prompt, prompt_sha) = system_prompt(ctx, &["search_plan"])?;
    let call = Call { purpose: "search_plan", prompt, prompt_sha, question: question.clone(), evidence: vec![], media: vec![] };
    let (value, request_id, response) = generate_json(ctx, invocation, &call, &system, &user, vec![], 800)?;
    // Drop rewrites that only repeat q0 or each other.
    let mut rewrites: Vec<String> = vec![];
    for row in value["rewrites"].as_array().cloned().unwrap_or_default() {
        let query = row["query"].as_str().unwrap_or("").trim().to_owned();
        if !query.is_empty() && query != text.trim() && !rewrites.contains(&query) && rewrites.len() < 3 {
            rewrites.push(query);
        }
    }
    let plan = json!({"question_id": question, "scope_id": scope, "rewritten_queries": rewrites, "constraint_ids": constraints});
    ctx.lock()?.runtime.validate_generation_output(invocation, &plan)?;
    let payload = json!({"plan": plan, "rewrites": value["rewrites"], "missing_facts": value["missing_facts"], "request_id": request_id,
        "prompt": format!("{}@{}", call.prompt, call.prompt_sha), "usage": usage(&response), "created_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "SearchPlanRef", &payload)?;
    Ok(completed(Some(result)))
}

// ------------------------------------------------------------- roles: context

/// A core-built context packet and the role it was built for.
pub(crate) struct Packet {
    pub role: String,
    pub question: String,
    pub evidence: Vec<String>,
    pub calculations: Vec<String>,
    pub goal: Option<(String, String, String)>,
    pub draft: Option<String>,
    pub payload: Value,
}

pub(crate) fn load_packet(ctx: &Ctx, id: &str, root: &str) -> Result<Packet> {
    let vault = ctx.lock()?;
    let (packet_root, role, payload): (String, String, String) = vault.db().query_row("SELECT root,role,payload FROM context_packets WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.ok_or(Error::Denied)?;
    if packet_root != root {
        return Err(Error::Denied);
    }
    let payload: Value = serde_json::from_str(&payload)?;
    let strings = |value: &Value| value.as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect::<Vec<_>>();
    let goal_text = |id: &Value| -> Result<String> {
        match id.as_str() {
            Some(id) => Ok(vault.db().query_row("SELECT text FROM goals WHERE id=? AND root=?", params![id, root], |r| r.get(0)).optional()?.ok_or(Error::Denied)?),
            None => Ok(String::new()),
        }
    };
    let goal = match payload["goal"]["goal_ref"].as_str() {
        Some(_) => Some((goal_text(&payload["goal"]["goal_ref"])?, goal_text(&payload["goal"]["acceptance_ref"])?, goal_text(&payload["goal"]["stop_ref"])?)),
        None => None,
    };
    Ok(Packet {
        role,
        question: payload["question_id"].as_str().unwrap_or("").to_owned(),
        evidence: strings(&payload["evidence_ids"]),
        calculations: strings(&payload["calculation_ids"]),
        goal,
        draft: payload["draft_ref"].as_str().map(str::to_owned),
        payload,
    })
}

/// A packet for a role's own generation: built for this very agent, not
/// just for an agent of the same role. A planned task's packet exists before
/// its agent does; the first agent that uses it claims it.
fn agent_packet(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<Packet> {
    let id = text_of(input, "context_packet_ref")?;
    let agent = invocation.agent_id().ok_or(Error::Denied)?;
    let packet = load_packet(ctx, &id, invocation.root_id())?;
    let vault = ctx.lock()?;
    let bound: String = vault.db().query_row("SELECT agent FROM context_packets WHERE id=?", [&id], |r| r.get(0))?;
    if bound == agent {
        return Ok(packet);
    }
    let claimed = bound.is_empty()
        && packet.payload["planned_by"].is_string()
        && vault.db().execute("UPDATE context_packets SET agent=? WHERE id=? AND root=? AND agent=''", params![agent, id, invocation.root_id()])? == 1;
    if !claimed {
        return Err(Error::Denied);
    }
    Ok(packet)
}

/// The registered role of a role invocation (never taken from model output).
fn agent_role(ctx: &Ctx, invocation: &Invocation) -> Result<crate::runtime::Role> {
    let vault = ctx.lock()?;
    let agent = invocation.agent_id().ok_or(Error::Denied)?;
    let handle = vault.runtime.agent(invocation.root_id(), agent)?;
    vault.runtime.registry.roles.get(handle.role()).cloned().ok_or(Error::Integrity)
}

fn goal_lines(goal: &Option<(String, String, String)>) -> String {
    match goal {
        Some((goal, acceptance, stop)) => format!("当前任务目标：{goal}\n验收条件：{acceptance}\n停止条件：{stop}\n"),
        None => String::new(),
    }
}

/// Results of `kind` produced in this root under the same owner invocation.
fn sibling_results(ctx: &Ctx, invocation: &Invocation, kind: &str) -> Result<Vec<(String, Value)>> {
    let Some(owner) = invocation.parent_id() else { return Ok(vec![]) };
    let vault = ctx.lock()?;
    let mut statement = vault.db().prepare(
        "SELECT r.id,r.payload FROM results r JOIN invocations i ON i.id=r.invocation JOIN objects o ON o.id=r.id WHERE r.kind=? AND i.root=? AND i.parent=? AND o.active=1 ORDER BY r.created_at,r.rowid",
    )?;
    let rows = statement.query_map(params![kind, invocation.root_id(), owner], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = vec![];
    for row in rows {
        let (id, payload) = row?;
        out.push((id, serde_json::from_str(&payload)?));
    }
    Ok(out)
}

// -------------------------------------------------------------- roles: plans

fn task_plan(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    if !generation_available(ctx, invocation)? {
        return Ok(requires_generation("任务规划需要已授权的生成接口。"));
    }
    let question = text_of(input, "question_id")?;
    let root = invocation.root_id().to_owned();
    let role = agent_role(ctx, invocation)?;
    if role.id != "planner" {
        return Err(Error::Denied);
    }
    let packet = agent_packet(ctx, invocation, input)?;
    if packet.role != "planner" || packet.question != question || packet.payload["goal"]["goal_ref"].as_str() != input["goal_ref"].as_str() {
        return Err(Error::Denied);
    }
    let catalog: Value = {
        let vault = ctx.lock()?;
        let payload: String = vault.db().query_row("SELECT text FROM goals WHERE id=? AND root=? AND kind='catalog'", params![text_of(input, "catalog_ref")?, root], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
        serde_json::from_str(&payload)?
    };
    let allowed: Vec<String> = catalog["roles"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let max_roles = catalog["max_roles"].as_u64().unwrap_or(1).clamp(1, 3) as usize;
    let text = question_text(ctx, &question)?;
    let evidence = evidence_texts(ctx, &packet.evidence)?;
    let user = format!(
        "原问题：{text}\n{}最多安排 {max_roles} 个角色；可安排的角色：{}。\n\n可用原文证据（摘录）：\n{}\n返回 JSON：{{\"tasks\": [{{\"node_id\": \"t1\", \"role_id\": \"…\", \"goal\": \"…\", \"acceptance\": \"…\", \"stop\": \"…\", \"depends_on\": []}}]}}",
        goal_lines(&packet.goal), allowed.join("、"), evidence_block(&evidence, 160)
    );
    let (system, prompt, prompt_sha) = system_prompt(ctx, &["task_plan"])?;
    let call = Call { purpose: "task_plan", prompt, prompt_sha, question: question.clone(), evidence: packet.evidence.clone(), media: vec![] };
    let (wire, request_id, response) = generate_json(ctx, invocation, &call, &system, &user, vec![], 1200)?;
    // The plan is checked by the core: known roles only, earlier dependencies
    // only (acyclic), within the limit; goals become core objects.
    let mut nodes = vec![];
    let mut seen: Vec<String> = vec![];
    for task in wire["tasks"].as_array().cloned().unwrap_or_default().into_iter().take(max_roles) {
        let node_id = task["node_id"].as_str().unwrap_or("").to_owned();
        let role_id = task["role_id"].as_str().unwrap_or("").to_owned();
        if seen.contains(&node_id) || !allowed.contains(&role_id) {
            continue;
        }
        let depends: Vec<String> = task["depends_on"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).filter(|d| seen.contains(d)).collect();
        let vault = ctx.lock()?;
        let goal_ref = |kind: &str, text: &str| -> Result<String> {
            let id = vault.register(match kind { "goal" => "GoalRef", "acceptance" => "AcceptanceRef", _ => "StopRef" }, "m2", Some(invocation.id()))?;
            vault.db().execute("INSERT INTO goals(id,root,kind,text,created_at) VALUES(?,?,?,?,?)", params![id, root, kind, text.chars().take(300).collect::<String>(), now_rfc3339()])?;
            Ok(id)
        };
        let goal = goal_ref("goal", task["goal"].as_str().unwrap_or(""))?;
        let acceptance = goal_ref("acceptance", task["acceptance"].as_str().unwrap_or(""))?;
        let stop = goal_ref("stop", task["stop"].as_str().unwrap_or(""))?;
        let packet_id = vault.register("ContextPacketRef", "m2", Some(invocation.id()))?;
        let packet_payload = json!({"role": role_id, "question_id": question, "goal": {"goal_ref": goal, "acceptance_ref": acceptance, "stop_ref": stop},
            "evidence_ids": packet.evidence, "calculation_ids": packet.calculations, "omitted_ids": packet.payload["omitted_ids"], "planned_by": invocation.id(),
            "source_generation": packet.payload["source_generation"], "created_at": now_rfc3339()});
        vault.db().execute("INSERT INTO context_packets(id,root,agent,role,payload,created_at) VALUES(?,?,'',?,?,?)", params![packet_id, root, role_id, packet_payload.to_string(), now_rfc3339()])?;
        nodes.push(json!({"node_id": node_id, "depends_on": depends, "role_id": role_id, "goal_ref": goal, "acceptance_ref": acceptance, "stop_ref": stop, "context_packet_ref": packet_id}));
        seen.push(node_id);
    }
    let plan = json!({"task_graph": nodes});
    ctx.lock()?.runtime.validate_generation_output(invocation, &plan)?;
    let payload = json!({"plan": plan, "wire": wire, "request_id": request_id, "prompt": format!("{}@{}", call.prompt, call.prompt_sha), "usage": usage(&response), "created_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "TaskPlanRef", &payload)?;
    let mut parts = completed(Some(result));
    if nodes.is_empty() {
        parts.status = "partial".into();
        warn(&mut parts, "empty_task_plan", "任务规划没有给出可执行的角色任务。");
    }
    Ok(parts)
}

// ------------------------------------------------------------ answer-compose

pub fn answer_compose(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    match invocation.purpose() {
        Some("role_analysis") => role_analysis(ctx, invocation, input),
        Some("role_check") => role_check(ctx, invocation, input),
        Some("context_brief") => context_brief(ctx, invocation, input),
        _ => draft_answer(ctx, invocation, input),
    }
}

/// Load the grounding model while the draft is generated (design: keep the
/// residency rule, only load earlier). The lease is released at once; the
/// pool's idle time keeps it warm for the verification that follows.
fn preload_grounding<T>(ctx: &Ctx, work: impl FnOnce() -> T) -> T {
    std::thread::scope(|scope| {
        scope.spawn(|| {
            if let Ok(lease) = ctx.services.lease("grounding", ctx.cancel) {
                drop(lease);
            }
        });
        work()
    })
}

fn draft_answer(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let question = text_of(input, "question_id")?;
    let bundle = text_of(input, "evidence_bundle_id")?;
    let calculations = refs_of(input, "calculation_ids")?;
    let draft = input["draft_ref"].as_str().map(str::to_owned);
    let report = input["validation_report_ref"].as_str().map(str::to_owned);
    if !generation_available(ctx, invocation)? {
        return Ok(requires_generation("生成回答需要已授权的生成接口；已交付本地检索到的原文依据。"));
    }
    let grant = grant_for_root(ctx, invocation.root_id())?.ok_or(Error::Denied)?;
    // A synthesizer also sees the role analyses and media observations made
    // under the same owner: candidate analyses, never evidence.
    let synthesizer = match invocation.agent_id() {
        Some(_) => {
            let role = agent_role(ctx, invocation)?;
            if role.id != "synthesizer" {
                return Err(Error::Denied);
            }
            true
        }
        None => false,
    };
    let analyses = if synthesizer { sibling_results(ctx, invocation, "RoleAnalysisRef")? } else { vec![] };
    let observations = if synthesizer { sibling_results(ctx, invocation, "MediaObservationRef")? } else { vec![] };
    let mut ids: Vec<String> = bundle_ids(ctx, &bundle)?.into_iter().filter(|id| !grant.excluded.contains(id)).collect();
    for (_, analysis) in &analyses {
        for claim in analysis["analysis"]["claims"].as_array().cloned().unwrap_or_default() {
            for id in claim["evidence_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str()) {
                if !ids.iter().any(|known| known == id) && !grant.excluded.iter().any(|e| e == id) && (ids.len() as i64) < grant.max_items {
                    ids.push(id.to_owned());
                }
            }
        }
    }
    ids.truncate(grant.max_items.max(0) as usize);
    let evidence = evidence_texts(ctx, &ids)?;
    let calc = calculation_text(ctx, &calculations)?;
    let text = question_text(ctx, &question)?;
    let labels: BTreeMap<String, String> = evidence.iter().map(|e| (e.label.clone(), e.evidence_id.clone())).collect();
    let label_of = |id: &str| labels.iter().find(|(_, v)| v.as_str() == id).map(|(k, _)| k.clone()).unwrap_or_else(|| "未提供".into());
    let mut user = format!("原问题：{text}\n\n原文证据：\n{}\n确定性计算结果：\n{}", evidence_block(&evidence, 1800), calculation_block(&calc));
    if !analyses.is_empty() {
        user.push_str("\n专业角色分析（候选分析，不是证据；只采信有证据编号支持的内容）：\n");
        for (_, analysis) in &analyses {
            user.push_str(&format!("〔{}〕\n", analysis["role_title"].as_str().unwrap_or("角色")));
            for claim in analysis["analysis"]["claims"].as_array().cloned().unwrap_or_default() {
                let cited: Vec<String> = claim["evidence_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str()).map(label_of).collect();
                user.push_str(&format!("- {}（依据：{}）\n", claim["text"].as_str().unwrap_or(""), if cited.is_empty() { "无".into() } else { cited.join("、") }));
            }
            for limitation in analysis["analysis"]["limitations"].as_array().cloned().unwrap_or_default() {
                user.push_str(&format!("- 局限：{}\n", limitation.as_str().unwrap_or("")));
            }
        }
    }
    if !observations.is_empty() {
        user.push_str("\n直接读取媒体的观察（与本机识别同源，是候选观察，不是独立证据）：\n");
        for (_, observation) in &observations {
            for row in observation["observation"]["observations"].as_array().cloned().unwrap_or_default() {
                user.push_str(&format!("- {}\n", row.as_str().unwrap_or("")));
            }
            for row in observation["comparison"]["disagreements"].as_array().cloned().unwrap_or_default() {
                user.push_str(&format!("- 与本机识别不一致：{}（直接读取：{}；本机识别：{}）\n", row["name"].as_str().unwrap_or(""), row["observed"].as_str().unwrap_or(""), row["extracted"].as_str().unwrap_or("未找到")));
            }
        }
    }
    let mut previous_version = 0u32;
    if let (Some(draft), Some(report)) = (&draft, &report) {
        let (previous, findings) = {
            let vault = ctx.lock()?;
            let (_, kind, previous) = vault.get_result(draft)?;
            let (_, report_kind, findings) = vault.get_result(report)?;
            if kind != "DraftRef" || report_kind != "ValidationReportRef" || findings["draft_ref"].as_str() != Some(draft.as_str()) {
                return Err(Error::Denied);
            }
            (previous, findings)
        };
        previous_version = previous["version"].as_u64().unwrap_or(0) as u32;
        user.push_str(&format!("\n上一版回答（JSON）：\n{}\n\n核查发现的问题：\n", previous["wire"]));
        for problem in findings["required_actions"].as_array().cloned().unwrap_or_default() {
            user.push_str(&format!("- {}：{}\n", problem["claim_id"].as_str().unwrap_or(""), problem["message"].as_str().unwrap_or("")));
        }
        // One structured challenge from the checker, if it reviewed this draft.
        for (_, challenge) in sibling_results(ctx, invocation, "ChallengeReportRef")?.iter().filter(|(_, c)| c["report"]["draft_ref"].as_str() == Some(draft.as_str())) {
            for row in challenge["report"]["challenges"].as_array().cloned().unwrap_or_default() {
                user.push_str(&format!("- {}（检查者，{}）：{}\n", row["claim_id"].as_str().unwrap_or(""), row["issue_type"].as_str().unwrap_or(""), row["proposed_action"].as_str().unwrap_or("")));
            }
        }
        user.push_str("\n只修正这些问题：删去或改写缺乏依据的内容，补上正确的证据编号；不要新增证据里没有的事实。输出新的完整 JSON。");
    } else {
        user.push_str("\n请生成回答 JSON：conclusion 用 1–3 句直接回答问题；basis 列出支撑结论的原文要点；calculations 只复述给出的计算结果；assumptions 写适用前提；missing_information 写资料里缺少、会影响结论的信息（资料外的规定写“需核实的规定”）；conflicts 写相互矛盾的证据或意见；next_steps 写可执行的下一步。每条 conclusion、basis、calculations 都在 evidence 中写出支持它的证据编号（如 \"E1\"）或计算编号（如 \"C1\"），正文不写编号。");
    }
    let evidence_ids: Vec<String> = evidence.iter().map(|e| e.evidence_id.clone()).collect();
    let (system, prompt, prompt_sha) = system_prompt(ctx, &["draft_answer"])?;
    let call = Call { purpose: "draft_answer", prompt, prompt_sha, question: question.clone(), evidence: evidence_ids.clone(), media: vec![] };
    let (wire, request_id, response) = if evidence.is_empty() {
        generate_json(ctx, invocation, &call, &system, &user, vec![], 2400)?
    } else {
        preload_grounding(ctx, || generate_json(ctx, invocation, &call, &system, &user, vec![], 2400))?
    };
    let calc_labels: BTreeMap<String, String> = calc.iter().map(|(label, _)| label.clone()).zip(calculations.iter().cloned()).collect();
    let mut rows: Vec<(String, Value)> = vec![];
    for (section, kind) in [("conclusion", "conclusion"), ("basis", "basis"), ("calculations", "calculation")] {
        for row in wire[section].as_array().cloned().unwrap_or_default() {
            rows.push((kind.into(), row));
        }
    }
    let (mut claims, invalid, removed) = map_claims(&rows, &labels, &calc_labels, 1);
    for step in wire["next_steps"].as_array().cloned().unwrap_or_default() {
        let claim_id = format!("c{}", claims.len() + 1);
        claims.push(json!({"claim_id": claim_id, "kind": "next_step", "text": step, "evidence_refs": [], "calculation_refs": []}));
    }
    let core = json!({"question_id": question, "claims": claims, "assumptions": wire["assumptions"], "conflicts": wire["conflicts"], "unknowns": wire["missing_information"]});
    ctx.lock()?.runtime.validate_generation_output(invocation, &core)?;
    let payload = json!({"draft": core, "wire": wire, "version": previous_version + 1, "insufficient_evidence": wire["insufficient_evidence"],
        "evidence_labels": labels, "calculation_labels": calc_labels, "invalid_citations": invalid, "labels_removed": removed, "evidence_bundle_id": bundle, "revised_from": draft,
        "analyses": analyses.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(), "observations": observations.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        "request_id": request_id, "prompt": format!("{}@{}", call.prompt, call.prompt_sha), "usage": usage(&response), "created_at": now_rfc3339()});
    let result = {
        let mut vault = ctx.lock()?;
        let result = vault.put_result(invocation.skill(), invocation.id(), "DraftRef", &payload)?;
        vault.runtime.bind_answer_draft(invocation, &request_id, &result)?;
        result
    };
    let mut parts = completed(Some(result));
    parts.evidence_refs = evidence_ids;
    Ok(parts)
}

fn role_analysis(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    if !generation_available(ctx, invocation)? {
        return Ok(requires_generation("角色分析需要已授权的生成接口。"));
    }
    let question = text_of(input, "question_id")?;
    let bundle = text_of(input, "evidence_bundle_id")?;
    let calculations = refs_of(input, "calculation_ids")?;
    let role = agent_role(ctx, invocation)?;
    let packet = agent_packet(ctx, invocation, input)?;
    if packet.role != role.id || packet.question != question || packet.payload["evidence_bundle_id"].as_str().is_some_and(|b| b != bundle)
        || calculations.iter().any(|c| !packet.calculations.contains(c))
    {
        return Err(Error::Denied);
    }
    let evidence = evidence_texts(ctx, &packet.evidence)?;
    let calc = calculation_text(ctx, &calculations)?;
    let text = question_text(ctx, &question)?;
    let tools: Vec<&str> = ["knowledge-search", "graph-relations", "evidence-rerank", "evidence-read", "finance-calculate"].into_iter().filter(|t| role.allowed_skills.iter().any(|a| a == t)).collect();
    let user = format!(
        "原问题：{text}\n{}可建议的工具：{}\n\n原文证据：\n{}\n确定性计算结果：\n{}\n返回 JSON：{{\"claims\": [{{\"kind\": \"conclusion|basis|calculation|next_step\", \"text\": \"…\", \"evidence\": [\"E1\"]}}], \"proposed_skills\": [], \"limitations\": [\"…\"]}}",
        goal_lines(&packet.goal), if tools.is_empty() { "无".to_owned() } else { tools.join("、") }, evidence_block(&evidence, 1800), calculation_block(&calc)
    );
    let role_prompt = format!("role.{}", role.id);
    let (system, prompt, prompt_sha) = system_prompt(ctx, &[&role_prompt, "role_analysis"])?;
    let call = Call { purpose: "role_analysis", prompt, prompt_sha, question: question.clone(), evidence: packet.evidence.clone(), media: vec![] };
    let (wire, request_id, response) = generate_json(ctx, invocation, &call, &system, &user, vec![], 1800)?;
    let labels: BTreeMap<String, String> = evidence.iter().map(|e| (e.label.clone(), e.evidence_id.clone())).collect();
    let calc_labels: BTreeMap<String, String> = calc.iter().map(|(label, _)| label.clone()).zip(calculations.iter().cloned()).collect();
    let rows: Vec<(String, Value)> = wire["claims"].as_array().cloned().unwrap_or_default().into_iter().map(|row| (row["kind"].as_str().unwrap_or("basis").to_owned(), row)).collect();
    let (claims, invalid, removed) = map_claims(&rows, &labels, &calc_labels, 1);
    let proposed: Vec<String> = wire["proposed_skills"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str()).filter(|s| tools.contains(s)).map(str::to_owned).collect();
    let analysis = json!({"question_id": question, "claims": claims, "proposed_skill_ids": proposed, "limitations": wire["limitations"]});
    ctx.lock()?.runtime.validate_generation_output(invocation, &analysis)?;
    let payload = json!({"analysis": analysis, "wire": wire, "role": role.id, "role_title": role.title, "packet": packet.payload, "evidence_labels": labels,
        "calculation_labels": calc_labels, "invalid_citations": invalid, "labels_removed": removed, "request_id": request_id,
        "prompt": format!("{}@{}", call.prompt, call.prompt_sha), "usage": usage(&response), "created_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "RoleAnalysisRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.evidence_refs = packet.evidence;
    if !invalid.is_empty() {
        warn(&mut parts, "role_invalid_citation", "角色分析引用了不存在的证据编号，已在结果中标出。");
    }
    Ok(parts)
}

fn role_check(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    if !generation_available(ctx, invocation)? {
        return Ok(requires_generation("回答检查需要已授权的生成接口。"));
    }
    let question = text_of(input, "question_id")?;
    let draft_ref = text_of(input, "draft_ref")?;
    let calculations = refs_of(input, "calculation_ids")?;
    let role = agent_role(ctx, invocation)?;
    if role.id != "evidence-checker" {
        return Err(Error::Denied);
    }
    let packet = agent_packet(ctx, invocation, input)?;
    if packet.role != role.id || packet.question != question || packet.draft.as_deref() != Some(draft_ref.as_str()) {
        return Err(Error::Denied);
    }
    let draft = {
        let vault = ctx.lock()?;
        let (_, kind, draft) = vault.get_result(&draft_ref)?;
        if kind != "DraftRef" {
            return Err(Error::Denied);
        }
        draft
    };
    let evidence = evidence_texts(ctx, &packet.evidence)?;
    let calc = calculation_text(ctx, &calculations)?;
    let labels: BTreeMap<String, String> = evidence.iter().map(|e| (e.label.clone(), e.evidence_id.clone())).collect();
    let calc_labels: BTreeMap<String, String> = calc.iter().map(|(label, _)| label.clone()).zip(calculations.iter().cloned()).collect();
    let label_of = |id: &str| labels.iter().chain(calc_labels.iter()).find(|(_, v)| v.as_str() == id).map(|(k, _)| k.clone()).unwrap_or_else(|| "未提供".into());
    let claim_ids: Vec<String> = draft["draft"]["claims"].as_array().cloned().unwrap_or_default().iter().filter(|c| c["kind"] != "next_step").filter_map(|c| c["claim_id"].as_str().map(str::to_owned)).collect();
    let mut claims_text = String::new();
    for claim in draft["draft"]["claims"].as_array().cloned().unwrap_or_default().iter().filter(|c| c["kind"] != "next_step") {
        let cited: Vec<String> = claim["evidence_refs"].as_array().cloned().unwrap_or_default().iter().chain(claim["calculation_refs"].as_array().cloned().unwrap_or_default().iter())
            .filter_map(|v| v.as_str()).map(label_of).collect();
        claims_text.push_str(&format!("[{}]（{}）{}｜引用：{}\n", claim["claim_id"].as_str().unwrap_or(""), claim["kind"].as_str().unwrap_or(""), claim["text"].as_str().unwrap_or(""), if cited.is_empty() { "无".into() } else { cited.join("、") }));
    }
    let text = question_text(ctx, &question)?;
    let user = format!(
        "原问题：{text}\n{}\n原文证据：\n{}\n确定性计算结果：\n{}\n待检查回答的声明：\n{}\n返回 JSON：{{\"challenges\": [{{\"claim_id\": \"c1\", \"issue_type\": \"unsupported\", \"evidence\": [\"E1\"], \"proposed_action\": \"…\"}}]}}",
        goal_lines(&packet.goal), evidence_block(&evidence, 1800), calculation_block(&calc), if claims_text.is_empty() { "（无）\n".into() } else { claims_text }
    );
    let (system, prompt, prompt_sha) = system_prompt(ctx, &["role_check"])?;
    let call = Call { purpose: "role_check", prompt, prompt_sha, question: question.clone(), evidence: packet.evidence.clone(), media: vec![] };
    let (wire, request_id, response) = generate_json(ctx, invocation, &call, &system, &user, vec![], 1500)?;
    let mut challenges = vec![];
    let mut dropped = 0;
    for row in wire["challenges"].as_array().cloned().unwrap_or_default() {
        let claim_id = row["claim_id"].as_str().unwrap_or("").trim().to_owned();
        if !claim_ids.contains(&claim_id) {
            dropped += 1;
            continue;
        }
        let refs: Vec<String> = row["evidence"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str()).filter_map(|l| labels.get(&l.trim().to_ascii_uppercase()).cloned()).collect();
        challenges.push(json!({"claim_id": claim_id, "issue_type": row["issue_type"], "evidence_refs": refs, "proposed_action": row["proposed_action"]}));
    }
    let report = json!({"draft_ref": draft_ref, "challenges": challenges});
    ctx.lock()?.runtime.validate_generation_output(invocation, &report)?;
    let payload = json!({"report": report, "wire": wire, "dropped": dropped, "packet": packet.payload, "evidence_labels": labels, "request_id": request_id,
        "prompt": format!("{}@{}", call.prompt, call.prompt_sha), "usage": usage(&response), "created_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ChallengeReportRef", &payload)?;
    Ok(completed(Some(result)))
}

/// A bounded, lossy brief of a packet's evidence and progress (core context
/// maintenance; counts toward the root's seven requests).
fn context_brief(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    if !generation_available(ctx, invocation)? {
        return Ok(requires_generation("上下文简报需要已授权的生成接口。"));
    }
    let root = invocation.root_id().to_owned();
    let packet = load_packet(ctx, &text_of(input, "context_packet_ref")?, &root)?;
    let target = input["target_tokens"].as_u64().unwrap_or(600).clamp(100, 1500);
    // Original records and progress items are core objects that point at
    // chunks and results of this root.
    let resolve = |field: &str, kind: &str| -> Result<Vec<String>> {
        let vault = ctx.lock()?;
        let mut out = vec![];
        for id in refs_of(input, field)? {
            let parent: Option<String> = vault.db().query_row("SELECT parent FROM objects WHERE id=? AND kind=? AND active=1", params![id, kind], |r| r.get(0)).optional()?.flatten();
            out.push(parent.ok_or(Error::Denied)?);
        }
        Ok(out)
    };
    let originals = resolve("original_record_refs", "OriginalRecordRef")?;
    let progress = resolve("progress_refs", "ProgressRef")?;
    if originals.iter().any(|id| !packet.evidence.contains(id)) {
        return Err(Error::Denied);
    }
    let evidence = evidence_texts(ctx, &originals)?;
    let mut progress_text = String::new();
    for id in &progress {
        let (_, kind, payload) = ctx.lock()?.get_result(id)?;
        let claims = match kind.as_str() {
            "RoleAnalysisRef" => payload["analysis"]["claims"].clone(),
            "DraftRef" => payload["draft"]["claims"].clone(),
            _ => return Err(Error::Denied),
        };
        for claim in claims.as_array().cloned().unwrap_or_default() {
            progress_text.push_str(&format!("- {}\n", claim["text"].as_str().unwrap_or("")));
        }
    }
    let question = packet.question.clone();
    let text = question_text(ctx, &question)?;
    let user = format!(
        "原问题：{text}\n{}简报长度不超过约 {target} 个汉字。\n\n原文证据：\n{}\n已有分析：\n{}\n返回 JSON：{{\"brief\": \"…\", \"pinned\": [\"E1\"], \"conflicts\": [], \"omitted\": []}}",
        goal_lines(&packet.goal), evidence_block(&evidence, 1800), if progress_text.is_empty() { "无\n".into() } else { progress_text }
    );
    let (system, prompt, prompt_sha) = system_prompt(ctx, &["context_brief"])?;
    let call = Call { purpose: "context_brief", prompt, prompt_sha, question, evidence: originals.clone(), media: vec![] };
    let (wire, request_id, response) = generate_json(ctx, invocation, &call, &system, &user, vec![], 1200)?;
    let labels: BTreeMap<String, String> = evidence.iter().map(|e| (e.label.clone(), e.evidence_id.clone())).collect();
    let map = |field: &str| -> Vec<String> { wire[field].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str()).filter_map(|l| labels.get(&l.trim().to_ascii_uppercase()).cloned()).collect() };
    let brief = json!({"original_record_refs": refs_of(input, "original_record_refs")?, "pinned_fact_refs": map("pinned"), "conflict_refs": map("conflicts"), "omitted_refs": map("omitted"),
        "brief": wire["brief"], "lossy": true});
    ctx.lock()?.runtime.validate_generation_output(invocation, &brief)?;
    let payload = json!({"brief": brief, "wire": wire, "packet": packet.payload, "request_id": request_id, "prompt": format!("{}@{}", call.prompt, call.prompt_sha),
        "usage": usage(&response), "created_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ContextBriefRef", &payload)?;
    Ok(completed(Some(result)))
}

// ------------------------------------------------------------- answer-grounding

/// Numbers that matter for verification: amounts, percentages and figures of
/// three or more digits. Small bare numbers ("7 日") are not compared because
/// the evidence may spell them in Chinese numerals.
pub fn key_numbers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = vec![];
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_ascii_digit() {
            let start = index;
            let mut raw = String::new();
            while index < chars.len() && (chars[index].is_ascii_digit() || chars[index] == ',' || chars[index] == '，' && index + 1 < chars.len() && chars[index + 1].is_ascii_digit() || chars[index] == '.') {
                if chars[index] != ',' && chars[index] != '，' {
                    raw.push(chars[index]);
                }
                index += 1;
            }
            let raw = raw.trim_end_matches('.').to_owned();
            // Skip dates; they are compared separately.
            let next = chars.get(index).copied();
            if matches!(next, Some('年') | Some('月') | Some('日') | Some('号') | Some('-') | Some('/')) || (start > 0 && matches!(chars[start - 1], '-' | '/')) {
                continue;
            }
            let multiplier = if next == Some('万') { 10_000.0 } else if next == Some('亿') { 100_000_000.0 } else { 1.0 };
            let marked = matches!(next, Some('元') | Some('万') | Some('亿') | Some('%') | Some('％')) || (start > 0 && matches!(chars[start - 1], '¥' | '￥'))
                || text[..text.char_indices().nth(start).map(|(i, _)| i).unwrap_or(0)].ends_with("人民币");
            let Ok(number) = raw.parse::<f64>() else { continue };
            let value = number * multiplier;
            if marked || value >= 100.0 {
                let formatted = if (value - value.round()).abs() < 1e-9 { format!("{}", value.round() as i64) } else { format!("{:.4}", value).trim_end_matches('0').trim_end_matches('.').to_owned() };
                if !out.contains(&formatted) {
                    out.push(formatted);
                }
            }
        } else {
            index += 1;
        }
    }
    out
}

/// Dates written as 2026年11月1日, 2026-11-01 or 2026/11/1, normalised.
pub fn key_dates(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = vec![];
    let mut index = 0;
    while index + 4 <= chars.len() {
        if chars[index..index + 4].iter().all(|c| c.is_ascii_digit()) && (index == 0 || !chars[index - 1].is_ascii_digit()) {
            let year: String = chars[index..index + 4].iter().collect();
            let mut cursor = index + 4;
            let separator = chars.get(cursor).copied();
            if matches!(separator, Some('年') | Some('-') | Some('/') | Some('.')) {
                cursor += 1;
                let month: String = chars[cursor..].iter().take_while(|c| c.is_ascii_digit()).collect();
                cursor += month.len();
                if !month.is_empty() && matches!(chars.get(cursor), Some('月') | Some('-') | Some('/') | Some('.')) {
                    cursor += 1;
                    let day: String = chars[cursor..].iter().take_while(|c| c.is_ascii_digit()).collect();
                    if let (Ok(m), Ok(d)) = (month.parse::<u32>(), day.parse::<u32>()) {
                        if (1..=12).contains(&m) && (1..=31).contains(&d) {
                            let date = format!("{year}-{m:02}-{d:02}");
                            if !out.contains(&date) {
                                out.push(date);
                            }
                            index = cursor + day.len();
                            continue;
                        }
                    }
                }
            }
        }
        index += 1;
    }
    out
}

pub fn answer_grounding(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    ctx.authorize(invocation, "local.grounding")?;
    let question = text_of(input, "question_id")?;
    let draft_ref = text_of(input, "draft_ref")?;
    let bundle = text_of(input, "evidence_bundle_id")?;
    let calculations = refs_of(input, "calculation_ids")?;
    let (draft, question_text) = {
        let vault = ctx.lock()?;
        let (_, kind, draft) = vault.get_result(&draft_ref)?;
        if kind != "DraftRef" || draft["draft"]["question_id"].as_str() != Some(question.as_str()) || draft["evidence_bundle_id"].as_str() != Some(bundle.as_str()) {
            return Err(Error::Denied);
        }
        let text: String = vault.db().query_row("SELECT text FROM questions WHERE id=?", [&question], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
        (draft, text)
    };
    // The passages the draft actually saw (its labels), never wider.
    let seen: Vec<String> = draft["evidence_labels"].as_object().map(|m| m.values().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let evidence = evidence_texts(ctx, &seen)?;
    let by_id: BTreeMap<String, &EvidenceText> = evidence.iter().map(|e| (e.evidence_id.clone(), e)).collect();
    let calc = calculation_text(ctx, &calculations)?;
    let calc_by_ref: BTreeMap<String, String> = calculations.iter().cloned().zip(calc.iter().map(|(_, v)| v.clone())).collect();
    let invalid: Vec<Value> = draft["invalid_citations"].as_array().cloned().unwrap_or_default();
    let mut rows = vec![];
    let mut actions = vec![];
    let mut checked = 0usize;
    let mut groundable = 0usize;
    for claim in draft["draft"]["claims"].as_array().cloned().unwrap_or_default() {
        let claim_id = claim["claim_id"].as_str().unwrap_or("").to_owned();
        let kind = claim["kind"].as_str().unwrap_or("basis").to_owned();
        let text = claim["text"].as_str().unwrap_or("").to_owned();
        if kind == "next_step" {
            rows.push(json!({"claim_id": claim_id, "kind": kind, "status": "advice", "checks": [], "signal": "not_applicable", "flagged": []}));
            continue;
        }
        groundable += 1;
        let refs: Vec<String> = claim["evidence_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        let calc_refs: Vec<String> = claim["calculation_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        let mut checks: Vec<&str> = vec![];
        if invalid.iter().any(|row| row["claim_id"] == claim_id.as_str()) {
            checks.push("unknown_citation");
        }
        if refs.is_empty() && calc_refs.is_empty() {
            checks.push("missing_citation");
        }
        let cited: Vec<&EvidenceText> = refs.iter().filter_map(|r| by_id.get(r).copied()).collect();
        let source_text: String = cited.iter().map(|e| e.text.as_str()).chain(calc_refs.iter().filter_map(|r| calc_by_ref.get(r).map(String::as_str))).collect::<Vec<_>>().join("\n");
        let source_numbers = key_numbers(&source_text);
        if !refs.is_empty() || !calc_refs.is_empty() {
            if key_numbers(&text).iter().any(|n| !source_numbers.contains(n)) {
                checks.push("amount_not_in_evidence");
            }
            let source_dates = key_dates(&source_text);
            if key_dates(&text).iter().any(|d| !source_dates.contains(d)) {
                checks.push("date_not_in_evidence");
            }
        }
        // mmBERT signal on the claim against its cited evidence (design 9.12).
        let mut signal = "not_run";
        let mut flagged: Vec<Value> = vec![];
        let mut ratio = 0.0f64;
        if !cited.is_empty() && checked < 12 {
            let unit = format!("{}:{claim_id}", invocation.id());
            let dispatched = ctx.lock()?.runtime.dispatch_grounding_unit(invocation, &unit, &draft_ref);
            match dispatched {
                Err(Error::Budget) => signal = "coverage_incomplete",
                Err(error) => return Err(error),
                Ok(_) => {
                    checked += 1;
                    let evidence_text: String = cited.iter().map(|e| format!("《{}》{}：{}", e.display, e.location, e.text)).collect::<Vec<_>>().join("\n");
                    match ctx.services.predict("grounding", &json!({"kind": "grounding", "question": question_text, "evidence": evidence_text, "answer": text}), ctx.cancel) {
                        Ok(output) => {
                            let tokens = output["answer_tokens"].as_array().cloned().unwrap_or_default();
                            let mut spans: Vec<(i64, i64, i64, i64)> = vec![];
                            for token in &tokens {
                                if token["unsupported_score"].as_f64().unwrap_or(0.0) >= 0.5 {
                                    let (s, e, us, ue) = (token["start"].as_i64().unwrap_or(0), token["end"].as_i64().unwrap_or(0), token["utf16_start"].as_i64().unwrap_or(0), token["utf16_end"].as_i64().unwrap_or(0));
                                    match spans.last_mut() {
                                        Some(last) if s <= last.1 + 1 => {
                                            last.1 = e;
                                            last.3 = ue;
                                        }
                                        _ => spans.push((s, e, us, ue)),
                                    }
                                }
                            }
                            let total = text.chars().count().max(1) as f64;
                            ratio = spans.iter().map(|(s, e, _, _)| (e - s) as f64).sum::<f64>() / total;
                            flagged = spans.iter().map(|(s, e, us, ue)| json!({"start": s, "end": e, "utf16_start": us, "utf16_end": ue})).collect();
                            signal = if flagged.is_empty() { "signal_clear" } else { "signal_flagged" };
                        }
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(Error::Worker(code)) if code.contains("coverage") || code.contains("4096") => signal = "coverage_incomplete",
                        Err(_) => signal = "execution_failed",
                    }
                }
            }
        } else if cited.is_empty() && !calc_refs.is_empty() {
            signal = "not_applicable";
        }
        let hard = checks.iter().any(|c| matches!(*c, "unknown_citation" | "missing_citation" | "amount_not_in_evidence" | "date_not_in_evidence"));
        let status = if hard || ratio >= 0.5 {
            "unsupported"
        } else if signal == "signal_flagged" {
            "partial"
        } else if matches!(signal, "coverage_incomplete" | "execution_failed" | "not_run") && calc_refs.is_empty() {
            "unchecked"
        } else {
            "supported"
        };
        let message = match checks.first() {
            Some(&"unknown_citation") => "引用了不存在的证据编号".to_owned(),
            Some(&"missing_citation") => "没有标注支持它的证据".to_owned(),
            Some(&"amount_not_in_evidence") => "其中的金额或数字在所引证据中找不到".to_owned(),
            Some(&"date_not_in_evidence") => "其中的日期在所引证据中找不到".to_owned(),
            _ if status == "unsupported" => "大部分内容缺乏所引证据支持".to_owned(),
            _ if status == "partial" => {
                let piece: String = flagged.first().map(|f| text.chars().skip(f["start"].as_u64().unwrap_or(0) as usize).take((f["end"].as_u64().unwrap_or(0) - f["start"].as_u64().unwrap_or(0)) as usize).collect()).unwrap_or_default();
                format!("部分内容缺乏证据支持（“{}”）", piece.chars().take(40).collect::<String>())
            }
            _ => String::new(),
        };
        if matches!(status, "unsupported" | "partial") {
            actions.push(json!({"claim_id": claim_id, "action": "revise_answer", "reason": checks.first().copied().unwrap_or(if status == "partial" { "signal_flagged" } else { "unsupported" }), "message": message}));
        }
        rows.push(json!({"claim_id": claim_id, "kind": kind, "status": status, "checks": checks, "signal": signal, "flagged": flagged, "flagged_ratio": ratio, "message": message}));
    }
    let run_status = if rows.iter().any(|r| r["status"] == "unchecked") { "partial" } else { "completed" };
    let payload = json!({"draft_ref": draft_ref, "version": draft["version"], "claims": rows, "coverage": {"checked": checked, "groundable": groundable},
        "required_actions": actions, "run_status": run_status, "detector": "KRLabsOrg/lettucedect-v2-mmbert-base", "threshold": 0.5, "created_at": now_rfc3339(),
        "note": "核查信号是辅助提示，不是正确概率；未标记不等于法律适用或结论完整。"});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ValidationReportRef", &payload)?;
    let mut parts = completed(Some(result));
    if run_status == "partial" {
        parts.status = "partial".into();
        warn(&mut parts, "coverage_incomplete", "部分内容未能完成核查（超出核查预算或输入过长）。");
    }
    Ok(parts)
}

/// Stable code → message for generation failures shown to the user.
pub(crate) fn remote_message(code: &str) -> String {
    match code {
        "output_truncated" => "生成内容被截断（超过单次输出上限），未采用。".into(),
        "invalid_structured_output" => "生成结果两次都不是规定的结构，未采用。".into(),
        other => format!("生成接口请求失败：{}。", probe::describe(other)),
    }
}
