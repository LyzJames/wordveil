//! Registered business handlers for the M1 local closed loop.
//!
//! Every handler runs under an invocation issued by the trusted runtime, checks
//! its capability before side effects, keeps vault locks short, and returns
//! parts that the core seals into the closed SkillResult schema. Child skills
//! go back through `Runtime::invoke_child`, which re-validates the declared
//! dependency, depth, refs and the intersected capability ceiling.
use crate::{
    Error, Result,
    runtime::{EnvelopeParts, Invocation},
    services::Services,
    store::Vault,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};

pub mod answers;
pub mod connections;
pub mod documents;
pub mod flows;
pub mod indexing;
pub mod matters;
pub mod outputs;
pub mod maintenance;
pub mod mediaflow;
pub mod observe;
pub mod workflows;

pub type VaultHandle = Arc<Mutex<Vault>>;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Progress {
    pub skill: String,
    pub stage: &'static str,
    pub done: u64,
    pub total: u64,
}

pub struct Ctx<'a> {
    pub vault: &'a VaultHandle,
    pub services: &'a Services,
    pub cancel: &'a AtomicBool,
    pub progress: &'a (dyn Fn(Progress) + Send + Sync),
}

impl Ctx<'_> {
    pub fn lock(&self) -> Result<MutexGuard<'_, Vault>> {
        self.vault.lock().map_err(|_| Error::Integrity)
    }
    pub fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Acquire) { Err(Error::Cancelled) } else { Ok(()) }
    }
    pub fn report(&self, invocation: &Invocation, stage: &'static str, done: u64, total: u64) {
        (self.progress)(Progress { skill: invocation.skill().into(), stage, done, total });
    }
    pub fn authorize(&self, invocation: &Invocation, capability: &str) -> Result<()> {
        self.lock()?.runtime.authorize(invocation, capability)
    }
}

pub fn completed(data_ref: Option<String>) -> EnvelopeParts {
    EnvelopeParts { status: "completed".into(), data_ref, ..Default::default() }
}

/// Skills whose execution is not part of this version (web research,
/// knowledge updates, external tools) report the precise reason instead of
/// fabricating a result.
fn unavailable() -> EnvelopeParts {
    EnvelopeParts {
        status: "needs_authorization".into(),
        warnings: vec![("network_capability_not_in_this_version".into(), "联网检索、公共知识更新与外部工具在后续版本提供，当前不发起这类网络请求。".into())],
        ..Default::default()
    }
}

pub fn run(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.check()?;
    match invocation.skill() {
        "document-import" => documents::import(ctx, invocation, input),
        "document-ocr" => documents::ocr(ctx, invocation, input),
        "evidence-read" => documents::evidence_read(ctx, invocation, input),
        "knowledge-index" => indexing::index(ctx, invocation, input),
        "knowledge-search" => indexing::search(ctx, invocation, input),
        "evidence-rerank" => indexing::rerank(ctx, invocation, input),
        "graph-relations" => indexing::graph(ctx, invocation, input),
        "matter-manage" => matters::manage(ctx, invocation, input),
        "finance-calculate" => matters::calculate(ctx, invocation, input),
        "report-export" => outputs::export(ctx, invocation, input),
        "diagnostics" => outputs::diagnostics(ctx, invocation, input),
        "vault-maintenance" => maintenance::maintain(ctx, invocation, input),
        "privacy-settings" => maintenance::privacy(ctx, invocation, input),
        "media-decode" => mediaflow::decode(ctx, invocation, input),
        "audio-transcribe" => mediaflow::transcribe(ctx, invocation, input),
        "image-review" => mediaflow::image_review(ctx, invocation, input),
        "audio-review" => mediaflow::audio_review(ctx, invocation, input),
        "video-review" => mediaflow::video_review(ctx, invocation, input),
        "media-observe" => observe::media_observe(ctx, invocation, input),
        "query-plan" => answers::query_plan(ctx, invocation, input),
        "answer-compose" => answers::answer_compose(ctx, invocation, input),
        "answer-grounding" => answers::answer_grounding(ctx, invocation, input),
        "connection-settings" => connections::settings(ctx, invocation, input),
        "lvjin-retrieval" => workflows::retrieval(ctx, invocation, input),
        "contract-review" | "loan-analysis" | "budget-analysis" => workflows::analysis(ctx, invocation, input),
        _ => Ok(unavailable()),
    }
}

/// Map an execution error to the unified status set (contract section 8).
pub fn failure(error: &Error) -> EnvelopeParts {
    let code = error.code();
    let (status, message) = match error {
        Error::Cancelled => ("cancelled", "任务已停止、撤销或空间已锁定。"),
        Error::NeedsInput("api_key") => ("needs_input", "该连接没有可用的 API Key，请在“生成接口”中重新填写。"),
        Error::NeedsInput(_) => ("needs_input", "需要补充信息后才能继续。"),
        Error::Worker(code) if code == "password_required" || code == "password_incorrect" => ("needs_input", "该文件已加密，需要正确的打开密码。"),
        Error::Worker(code) if code == "finance_loan_perspective_ambiguous" => ("needs_input", "金额记录里既有借出也有收到借款，无法判断你是出借人还是借款人；请在计算页选择视角后再算。"),
        Error::Worker(code) if code == "finance_loan_perspective_unknown" => ("needs_input", "金额记录里没有“借出”或“收到借款”，无法判断你是出借人还是借款人；请在计算页选择视角后再算。"),
        Error::Integrity => ("failed", "完整性校验失败：资源、数据或索引与记录不一致。"),
        Error::Denied => ("failed", "无权访问该对象，或对象不在当前范围内。"),
        Error::Contract => ("failed", "请求不符合正式合同。"),
        Error::Budget => ("partial", "已达到根任务预算上限。"),
        Error::Remote(remote) => return EnvelopeParts { status: "failed".into(), warnings: vec![(code, answers::remote_message(remote))], ..Default::default() },
        _ => ("failed", "处理失败；已保留已完成的部分。"),
    };
    EnvelopeParts { status: status.into(), warnings: vec![(code, message.into())], ..Default::default() }
}

/// Run a registered invocation and seal the result envelope in the core.
pub fn execute(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<Value> {
    let started = std::time::Instant::now();
    let parts = run(ctx, invocation, input).unwrap_or_else(|error| failure(&error));
    ctx.services.log.event(
        "skill.finished",
        &[
            ("skill", crate::logging::Field::Status(invocation.skill().into())),
            ("status", crate::logging::Field::Status(parts.status.clone())),
            ("ms", crate::logging::Field::Millis(started.elapsed().as_millis() as u64)),
            ("depth", crate::logging::Field::Count(i64::from(invocation.depth()))),
        ],
    );
    ctx.lock()?.runtime.envelope(invocation, &parts)
}

/// Invoke a declared child skill through the runtime and execute it.
pub fn child(ctx: &Ctx, parent: &Invocation, skill: &str, input: Value) -> Result<(Invocation, Value)> {
    ctx.check()?;
    let invocation = { ctx.lock()?.runtime.invoke_child(parent, skill, &input)? };
    let envelope = execute(ctx, &invocation, &input)?;
    Ok((invocation, envelope))
}

pub fn status_of(envelope: &Value) -> &str {
    envelope["status"].as_str().unwrap_or("failed")
}

pub fn data_of(envelope: &Value) -> Option<String> {
    envelope["data_ref"].as_str().map(str::to_owned)
}

/// Warning rows copied from a child into the parent's coverage, codes only.
pub fn child_warnings(envelope: &Value) -> Vec<(String, String)> {
    envelope["warnings"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|w| Some((w["code"].as_str()?.to_owned(), w["message"].as_str()?.to_owned()))).collect())
        .unwrap_or_default()
}

pub fn text_of(value: &Value, field: &str) -> Result<String> {
    value[field].as_str().map(str::to_owned).ok_or(Error::Contract)
}

pub fn refs_of(value: &Value, field: &str) -> Result<Vec<String>> {
    value[field]
        .as_array()
        .ok_or(Error::Contract)?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or(Error::Contract))
        .collect()
}

pub fn summary(rows: &[(&str, Value)]) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in rows {
        map.insert((*key).into(), value.clone());
    }
    Value::Object(map)
}

pub fn warn(parts: &mut EnvelopeParts, code: &str, message: &str) {
    parts.warnings.push((code.into(), message.into()));
}

pub fn empty() -> Value {
    json!({})
}
