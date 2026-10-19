//! LangGraph flow runs (M2 second segment; docs research/M2-架构审查与编排选型.md §8).
//!
//! One orchestrator process (no network, AppContainer) owns the control flow
//! of one root task. It reaches the core only through typed JSON-RPC
//! (`lvjin.orchestrator/1`): every request is attributed to this run's root
//! and to owners the run opened, and goes through the same runtime checks as a
//! handler's own child calls. The orchestrator sees references, counts and
//! statuses, never document text. Receipts make each state-changing call
//! idempotent, so a continued run replays finished steps instead of repeating
//! them; a generation request whose outcome is unknown is never resent.
use super::*;
use crate::{isolation::IsolatedProcess, runtime::AgentHandle, vault::now_rfc3339};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};

const PROTOCOL: &str = "lvjin.orchestrator/1";
const MAX_REQUESTS: i64 = 400;
const MAX_EVENTS: i64 = 600;
const MAX_STORED_BYTES: i64 = 64 * 1024 * 1024;
const MAX_IN_FLIGHT: usize = 4;
const RUN_DEADLINE: Duration = Duration::from_secs(45 * 60);
const READY_DEADLINE: Duration = Duration::from_secs(120);
/// Skills whose execution may send a generation request. An interrupted call
/// has an unknown outcome: it is counted and never replayed.
const GENERATION_SKILLS: &[&str] = &["query-plan", "answer-compose", "media-observe"];
/// Composite skills a flow may open as the owner of further steps.
const OPENABLE: &[&str] = &["lvjin-retrieval"];
/// Analysis roles whose packets are built from fixed-flow goals or a plan.
const ANALYSTS: &[&str] = &["evidence-researcher", "legal-analyst", "finance-analyst"];
/// Evidence passages a role packet carries before the rest is listed as omitted.
const PACKET_ITEMS: usize = 8;
/// Rough evidence budget of one packet (characters of passage text).
const PACKET_CHARS: usize = 14_000;

pub struct Outcome {
    pub run: String,
    pub output: Value,
    /// Warnings of the steps the core executed for this run (core text only).
    pub warnings: Vec<(String, String)>,
}

pub(crate) struct RpcError {
    code: i64,
    message: String,
}

fn rpc(code: i64, message: &str) -> RpcError {
    RpcError { code, message: message.into() }
}

impl From<Error> for RpcError {
    fn from(error: Error) -> Self {
        match error {
            Error::Denied => rpc(-32010, "denied"),
            Error::Budget => rpc(-32011, "budget"),
            Error::Cancelled => rpc(-32012, "cancelled"),
            Error::Contract => rpc(-32013, "contract"),
            Error::Conflict => rpc(-32015, "conflict"),
            Error::NeedsInput(_) => rpc(-32016, "needs_input"),
            other => rpc(-32000, &crate::sanitize_code(&other.code().replace(':', "_"))),
        }
    }
}
impl From<rusqlite::Error> for RpcError {
    fn from(error: rusqlite::Error) -> Self {
        Error::from(error).into()
    }
}
impl From<serde_json::Error> for RpcError {
    fn from(error: serde_json::Error) -> Self {
        Error::from(error).into()
    }
}

type Reply = std::result::Result<Value, RpcError>;

fn invalid() -> RpcError {
    rpc(-32602, "invalid_params")
}

/// Parameters must have exactly the documented keys.
fn fields<'p>(params: &'p Value, allowed: &[&str]) -> std::result::Result<&'p Map<String, Value>, RpcError> {
    let object = params.as_object().ok_or_else(invalid)?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid());
    }
    Ok(object)
}

fn text<'p>(params: &'p Map<String, Value>, name: &str, max: usize) -> std::result::Result<&'p str, RpcError> {
    params.get(name).and_then(Value::as_str).filter(|v| !v.is_empty() && v.len() <= max).ok_or_else(invalid)
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 160 && key.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-'))
}

/// A serialised LangGraph value: [type, base64]. Opaque to the core.
fn typed(value: Option<&Value>) -> std::result::Result<(String, usize), RpcError> {
    let pair = value.and_then(Value::as_array).filter(|p| p.len() == 2).ok_or_else(invalid)?;
    let kind = pair[0].as_str().filter(|k| !k.is_empty() && k.len() <= 32 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).ok_or_else(invalid)?;
    let data = pair[1].as_str().filter(|d| d.len() <= 8 * 1024 * 1024 && d.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))).ok_or_else(invalid)?;
    if kind == "pickle" {
        return Err(rpc(-32013, "contract"));
    }
    Ok((serde_json::to_string(pair)?, kind.len() + data.len()))
}

struct Run<'r, 'a> {
    ctx: &'r Ctx<'a>,
    id: String,
    root: String,
    question: String,
    base: Invocation,
    owners: Mutex<HashMap<String, Invocation>>,
    agents: Mutex<HashMap<String, AgentHandle>>,
    warnings: Mutex<Vec<(String, String)>>,
}

/// Run (or continue) `flow` for `owner`. `inputs` builds the inputs of a new
/// run only; a continued run keeps the inputs and checkpoints it stored.
pub fn run(ctx: &Ctx, owner: &Invocation, owner_input: &Value, flow: &str, inputs: impl FnOnce() -> Result<Value>) -> Result<Outcome> {
    let root = owner.root_id().to_owned();
    let resuming: Option<(String, String, i64)> = ctx
        .lock()?
        .db()
        .query_row("SELECT id,inputs,attempts FROM flow_runs WHERE root=? AND owner=? AND flow=? AND status='resuming'", params![root, owner.id(), flow], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let (id, inputs, attempt, resume) = match resuming {
        Some((id, inputs, attempts)) => {
            ctx.lock()?.db().execute("UPDATE flow_runs SET status='running',reason=NULL,updated_at=? WHERE id=?", params![now_rfc3339(), id])?;
            (id, serde_json::from_str::<Value>(&inputs)?, attempts, true)
        }
        None => {
            let inputs = inputs()?;
            let id = format!("run_{}", uuid::Uuid::new_v4().simple());
            let generation = super::indexing::current_generation(ctx)?;
            let vault = ctx.lock()?;
            let now = now_rfc3339();
            vault.db().execute(
                "INSERT INTO flow_runs(id,root,owner,owner_input,skill,flow,inputs,snapshot_generation,status,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,'running',?,?)",
                params![id, root, owner.id(), owner_input.to_string(), owner.skill(), flow, inputs.to_string(), generation, now, now],
            )?;
            vault.db().execute("INSERT INTO flow_owners(run,invocation,input) VALUES(?,?,?)", params![id, owner.id(), owner_input.to_string()])?;
            (id, inputs, 1, false)
        }
    };
    let question = inputs["question_id"].as_str().ok_or(Error::Contract)?.to_owned();
    let run = Run { ctx, id: id.clone(), root: root.clone(), question, base: owner.clone(), owners: Mutex::new(HashMap::from([(owner.id().to_owned(), owner.clone())])), agents: Mutex::new(HashMap::new()), warnings: Mutex::new(vec![]) };
    let outcome = run.execute(flow, &inputs, resume, attempt);
    let (state, reason, output) = match &outcome {
        Ok(output) => ("completed", None, Some(output.to_string())),
        Err(Error::Cancelled) => {
            let stopped: bool = ctx.lock()?.db().query_row("SELECT cancelled FROM roots WHERE id=?", [&root], |r| r.get(0)).optional()?.unwrap_or(true);
            if stopped { ("cancelled", Some("stopped".to_owned()), None) } else { ("interrupted", Some("interrupted".to_owned()), None) }
        }
        Err(Error::Worker(code)) if code == "orchestrator_exit" => ("interrupted", Some(code.clone()), None),
        Err(error) => ("failed", Some(error.code()), None),
    };
    ctx.lock()?.db().execute("UPDATE flow_runs SET status=?,reason=?,output=COALESCE(?,output),updated_at=? WHERE id=?", params![state, reason, output, now_rfc3339(), id])?;
    ctx.services.log.event("flow.finished", &[("flow", crate::logging::Field::Status(flow.into())), ("status", crate::logging::Field::Status(state.into())), ("attempt", crate::logging::Field::Count(attempt))]);
    let warnings = run.warnings.into_inner().map_err(|_| Error::Integrity)?;
    outcome.map(|output| Outcome { run: id, output, warnings })
}

impl Run<'_, '_> {
    fn execute(&self, flow: &str, inputs: &Value, resume: bool, attempt: i64) -> Result<Value> {
        let scratch = self.ctx.services.scratch("orchestrator")?;
        let result = (|| {
            let mut process = IsolatedProcess::spawn(self.ctx.services.orchestrator_policy(scratch.clone())?)?;
            let ready = match process.receive(self.ctx.cancel, Instant::now() + READY_DEADLINE) {
                Ok(ready) => ready,
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(_) => {
                    // Test builds keep the child's exit code and stderr for
                    // diagnosis; production builds never copy them anywhere.
                    #[cfg(feature = "m0-harness")]
                    eprintln!("orchestrator diagnostic: start failed: {}", process.synthetic_diagnostics());
                    return Err(Error::Worker("orchestrator_start_failed".into()));
                }
            };
            let version = ready["params"]["flows"][flow].as_str().map(str::to_owned);
            if ready["jsonrpc"] != "2.0" || ready["method"] != "ready" || ready["params"]["protocol"] != PROTOCOL || version.is_none() {
                process.terminate()?;
                return Err(Error::Integrity);
            }
            self.ctx.lock()?.db().execute("UPDATE flow_runs SET flow_version=? WHERE id=?", params![version, self.id])?;
            let deadline = Instant::now() + RUN_DEADLINE;
            let request = json!({"jsonrpc": "2.0", "id": "c1", "method": "flow.run", "params": {"run_id": self.id, "flow": flow, "inputs": inputs, "resume": resume, "attempt": attempt}});
            process.send(&request, self.ctx.cancel, deadline)?;
            let outcome = self.serve(&mut process, deadline);
            let _ = process.terminate();
            outcome
        })();
        let _ = std::fs::remove_dir_all(&scratch);
        result
    }

    /// Answer the orchestrator's requests (at most four in flight) until it
    /// returns the flow result, exits, misbehaves or the task is cancelled.
    fn serve(&self, process: &mut IsolatedProcess, deadline: Instant) -> Result<Value> {
        std::thread::scope(|scope| {
            let (done, finished) = mpsc::channel::<(Value, Reply)>();
            let mut queue: VecDeque<(Value, String, Value)> = VecDeque::new();
            let mut in_flight = 0usize;
            let mut received = 0i64;
            loop {
                while let Ok((id, reply)) = finished.try_recv() {
                    in_flight -= 1;
                    let message = match reply {
                        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": error.code, "message": error.message}}),
                    };
                    // An orchestrator that stops reading cannot stall the task.
                    process.send(&message, self.ctx.cancel, deadline.min(Instant::now() + Duration::from_secs(60)))?;
                }
                while in_flight < MAX_IN_FLIGHT {
                    let Some((id, method, params)) = queue.pop_front() else { break };
                    in_flight += 1;
                    let done = done.clone();
                    scope.spawn(move || {
                        let reply = self.dispatch(&method, &params);
                        let _ = done.send((id, reply));
                    });
                }
                if Instant::now() >= deadline {
                    process.terminate()?;
                    return Err(Error::Worker("flow_timeout".into()));
                }
                let message = match process.poll(self.ctx.cancel, Duration::from_millis(20)) {
                    Ok(Some(message)) => message,
                    Ok(None) => continue,
                    Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Worker("orchestrator_exit".into())),
                    Err(Error::Json(_)) => return Err(Error::Worker("protocol_violation".into())),
                    Err(error) => return Err(error),
                };
                if message["jsonrpc"] != "2.0" {
                    return Err(Error::Worker("protocol_violation".into()));
                }
                match (message.get("id"), message.get("method").and_then(Value::as_str)) {
                    (Some(id), Some(method)) if id.is_string() || id.is_u64() => {
                        received += 1;
                        if received > MAX_REQUESTS {
                            return Err(Error::Worker("flow_request_limit".into()));
                        }
                        queue.push_back((id.clone(), method.to_owned(), message.get("params").cloned().unwrap_or(Value::Null)));
                    }
                    (None, Some("progress")) => self.progress(&message["params"])?,
                    (Some(id), None) if id == "c1" => {
                        if let Some(error) = message.get("error") {
                            let code = crate::sanitize_code(error["message"].as_str().unwrap_or(""));
                            return Err(if code == "cancelled" { Error::Cancelled } else { Error::Worker(code) });
                        }
                        let result = message.get("result").ok_or(Error::Worker("protocol_violation".into()))?;
                        if result["status"] != "completed" || !result["output"].is_object() {
                            return Err(Error::Worker("protocol_violation".into()));
                        }
                        return Ok(result["output"].clone());
                    }
                    _ => return Err(Error::Worker("protocol_violation".into())),
                }
            }
        })
    }

    fn dispatch(&self, method: &str, params: &Value) -> Reply {
        if self.ctx.cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(rpc(-32012, "cancelled"));
        }
        match method {
            "checkpoint.load" => self.checkpoint_load(fields(params, &[])?),
            "checkpoint.put" => self.checkpoint_put(fields(params, &["ns", "id", "parent", "checkpoint", "metadata", "blobs"])?),
            "checkpoint.put_writes" => self.checkpoint_writes(fields(params, &["ns", "checkpoint_id", "task_id", "task_path", "writes"])?),
            "invocation.open" => {
                let p = fields(params, &["key", "parent", "skill", "input", "finalizer"])?;
                self.keyed(method, p, false, || self.open(p))
            }
            "skill.invoke" => {
                let p = fields(params, &["key", "owner", "skill", "input"])?;
                let external = GENERATION_SKILLS.contains(&text(p, "skill", 64)?);
                self.keyed(method, p, external, || self.invoke(p))
            }
            "role.register" => {
                let p = fields(params, &["key", "owner", "role"])?;
                self.keyed(method, p, false, || self.register(p))
            }
            "context.packet" => {
                let p = fields(params, &["key", "owner", "agent", "spec"])?;
                let external = p.get("spec").and_then(|s| s.get("brief")).and_then(Value::as_bool).unwrap_or(false);
                self.keyed(method, p, external, || self.packet(p))
            }
            "role.generate" => {
                let p = fields(params, &["key", "owner", "agent", "input"])?;
                self.keyed(method, p, true, || self.generate(p))
            }
            "role.tool" => {
                let p = fields(params, &["key", "owner", "agent", "skill", "input"])?;
                self.keyed(method, p, false, || self.tool(p))
            }
            "invocation.close" => {
                let p = fields(params, &["key", "owner", "output"])?;
                self.keyed(method, p, false, || self.close(p))
            }
            _ => Err(rpc(-32601, "method_not_found")),
        }
    }

    /// Idempotent execution. A finished key returns its recorded reply; an
    /// interrupted generation call reports `outcome_unknown` and is not run
    /// again under that key; an interrupted local call runs again.
    fn keyed(&self, method: &str, params: &Map<String, Value>, external: bool, work: impl FnOnce() -> Result<Value>) -> Reply {
        let key = params.get("key").and_then(Value::as_str).filter(|k| valid_key(k)).ok_or_else(invalid)?;
        let mut rest = params.clone();
        rest.remove("key");
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&json!({"method": method, "params": rest}))?));
        {
            let vault = self.ctx.lock()?;
            let existing: Option<(String, String, Option<String>, bool)> = vault
                .db()
                .query_row("SELECT params_sha256,state,response,external FROM flow_receipts WHERE run=? AND key=?", params![self.id, key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .optional()?;
            match existing {
                Some((hash, ..)) if hash != digest => return Err(rpc(-32015, "receipt_conflict")),
                Some((_, state, Some(response), _)) if state == "completed" => return Ok(serde_json::from_str(&response)?),
                Some((_, state, Some(response), _)) if state == "failed" => {
                    let stored: Value = serde_json::from_str(&response)?;
                    return Err(RpcError { code: stored["code"].as_i64().unwrap_or(-32000), message: crate::sanitize_code(stored["message"].as_str().unwrap_or("")) });
                }
                Some((_, _, _, true)) => {
                    vault.db().execute("UPDATE flow_receipts SET state='unknown',finished_at=? WHERE run=? AND key=?", params![now_rfc3339(), self.id, key])?;
                    return Err(rpc(-32014, "outcome_unknown"));
                }
                Some(_) => {
                    vault.db().execute("UPDATE flow_receipts SET state='started',started_at=? WHERE run=? AND key=?", params![now_rfc3339(), self.id, key])?;
                }
                None => {
                    vault.db().execute(
                        "INSERT INTO flow_receipts(run,key,method,params_sha256,external,state,started_at) VALUES(?,?,?,?,?,'started',?)",
                        params![self.id, key, method, digest, external, now_rfc3339()],
                    )?;
                }
            }
        }
        match work() {
            // A step cut off by an interruption is not a result: its receipt
            // stays "started" (local steps rerun; generation becomes unknown).
            Ok(value) if value["envelope"]["status"] == "cancelled" => Err(rpc(-32012, "cancelled")),
            Ok(value) => {
                self.ctx.lock()?.db().execute("UPDATE flow_receipts SET state='completed',response=?,finished_at=? WHERE run=? AND key=?", params![value.to_string(), now_rfc3339(), self.id, key])?;
                Ok(value)
            }
            // Interrupted mid-call: the receipt stays "started".
            Err(Error::Cancelled) => Err(rpc(-32012, "cancelled")),
            Err(error) => {
                let error = RpcError::from(error);
                let stored = json!({"code": error.code, "message": error.message});
                self.ctx.lock()?.db().execute("UPDATE flow_receipts SET state='failed',response=?,finished_at=? WHERE run=? AND key=?", params![stored.to_string(), now_rfc3339(), self.id, key])?;
                Err(error)
            }
        }
    }

    /// An owner this run opened (reloaded from the ledger after a restart).
    fn owner(&self, id: &str) -> Result<Invocation> {
        if let Some(found) = self.owners.lock().map_err(|_| Error::Integrity)?.get(id) {
            return Ok(found.clone());
        }
        let invocation = {
            let vault = self.ctx.lock()?;
            let known: bool = vault.db().query_row("SELECT EXISTS(SELECT 1 FROM flow_owners WHERE run=? AND invocation=?)", params![self.id, id], |r| r.get(0))?;
            if !known {
                return Err(Error::Denied);
            }
            vault.runtime.reload_invocation(id)?
        };
        if invocation.root_id() != self.root {
            return Err(Error::Denied);
        }
        self.owners.lock().map_err(|_| Error::Integrity)?.insert(id.into(), invocation.clone());
        Ok(invocation)
    }

    fn agent(&self, id: &str) -> Result<AgentHandle> {
        if let Some(found) = self.agents.lock().map_err(|_| Error::Integrity)?.get(id) {
            return Ok(found.clone());
        }
        let agent = self.ctx.lock()?.runtime.agent(&self.root, id)?;
        self.owner(agent.owner_id())?;
        self.agents.lock().map_err(|_| Error::Integrity)?.insert(id.into(), agent.clone());
        Ok(agent)
    }

    fn collect(&self, envelope: &Value) -> Result<()> {
        let mut seen = self.warnings.lock().map_err(|_| Error::Integrity)?;
        for row in child_warnings(envelope) {
            if !seen.iter().any(|(code, _)| *code == row.0) {
                seen.push(row);
            }
        }
        Ok(())
    }

    fn open(&self, p: &Map<String, Value>) -> Result<Value> {
        let parent = self.owner(p.get("parent").and_then(Value::as_str).ok_or(Error::Contract)?)?;
        let skill = p.get("skill").and_then(Value::as_str).filter(|s| OPENABLE.contains(s)).ok_or(Error::Denied)?;
        let input = p.get("input").filter(|v| v.is_object()).ok_or(Error::Contract)?;
        let finalizer = p.get("finalizer").and_then(Value::as_bool).ok_or(Error::Contract)?;
        let child = {
            let mut vault = self.ctx.lock()?;
            let child = vault.runtime.invoke_child(&parent, skill, input)?;
            if finalizer {
                vault.runtime.transfer_finalizer(&parent, &child)?;
            }
            vault.db().execute("INSERT INTO flow_owners(run,invocation,input) VALUES(?,?,?)", params![self.id, child.id(), input.to_string()])?;
            child
        };
        self.owners.lock().map_err(|_| Error::Integrity)?.insert(child.id().into(), child.clone());
        Ok(json!({"owner": child.id()}))
    }

    fn invoke(&self, p: &Map<String, Value>) -> Result<Value> {
        let owner = self.owner(p.get("owner").and_then(Value::as_str).ok_or(Error::Contract)?)?;
        let skill = p.get("skill").and_then(Value::as_str).ok_or(Error::Contract)?;
        let input = p.get("input").filter(|v| v.is_object()).ok_or(Error::Contract)?.clone();
        let (_, envelope) = child(self.ctx, &owner, skill, input)?;
        self.collect(&envelope)?;
        let view = view_of(self.ctx, &envelope)?;
        Ok(json!({"envelope": envelope, "view": view}))
    }

    fn register(&self, p: &Map<String, Value>) -> Result<Value> {
        let owner = self.owner(p.get("owner").and_then(Value::as_str).ok_or(Error::Contract)?)?;
        let role = p.get("role").and_then(Value::as_str).ok_or(Error::Contract)?;
        let agent = self.ctx.lock()?.runtime.register_role(&owner, role)?;
        self.agents.lock().map_err(|_| Error::Integrity)?.insert(agent.id().into(), agent.clone());
        Ok(json!({"agent": agent.id()}))
    }

    fn role_pair(&self, p: &Map<String, Value>) -> Result<(Invocation, AgentHandle)> {
        let owner = self.owner(p.get("owner").and_then(Value::as_str).ok_or(Error::Contract)?)?;
        let agent = self.agent(p.get("agent").and_then(Value::as_str).ok_or(Error::Contract)?)?;
        if agent.owner_id() != owner.id() {
            return Err(Error::Denied);
        }
        Ok((owner, agent))
    }

    /// A role's generation step through its trusted entry and purpose.
    fn generate(&self, p: &Map<String, Value>) -> Result<Value> {
        let (owner, agent) = self.role_pair(p)?;
        let input = p.get("input").filter(|v| v.is_object()).ok_or(Error::Contract)?.clone();
        let invocation = self.ctx.lock()?.runtime.invoke_role_generation(&owner, &agent, &input)?;
        let envelope = execute(self.ctx, &invocation, &input)?;
        self.collect(&envelope)?;
        let view = view_of(self.ctx, &envelope)?;
        Ok(json!({"envelope": envelope, "view": view}))
    }

    /// A tool a role may use: role whitelist ∩ owner dependencies ∩ grants.
    fn tool(&self, p: &Map<String, Value>) -> Result<Value> {
        let (owner, agent) = self.role_pair(p)?;
        let skill = p.get("skill").and_then(Value::as_str).ok_or(Error::Contract)?;
        let input = p.get("input").filter(|v| v.is_object()).ok_or(Error::Contract)?.clone();
        let invocation = self.ctx.lock()?.runtime.invoke_role_tool(&owner, &agent, skill, &input)?;
        let envelope = execute(self.ctx, &invocation, &input)?;
        self.collect(&envelope)?;
        let view = view_of(self.ctx, &envelope)?;
        Ok(json!({"envelope": envelope, "view": view}))
    }

    /// Core-built context packet (design 8): the role's goal from locked
    /// resources or from a validated plan, a bounded projection of the
    /// evidence the core retrieved, calculations, and what was left out.
    fn packet(&self, p: &Map<String, Value>) -> Result<Value> {
        let (owner, agent) = self.role_pair(p)?;
        let spec = p.get("spec").and_then(Value::as_object).ok_or(Error::Contract)?;
        if spec.keys().any(|k| !["kind", "goal", "evidence_bundle_id", "calculation_ids", "draft_ref", "brief"].contains(&k.as_str())) {
            return Err(Error::Contract);
        }
        let kind = spec.get("kind").and_then(Value::as_str).ok_or(Error::Contract)?;
        let role = agent.role().to_owned();
        let goal_id = match (kind, role.as_str()) {
            ("planner", "planner") => "deep_answer.planner".to_owned(),
            ("check", "evidence-checker") => "check".to_owned(),
            ("media", "media-reader") => "media.first_read".to_owned(),
            ("role", r) if ANALYSTS.contains(&r) => {
                let goal = spec.get("goal").and_then(Value::as_str).ok_or(Error::Contract)?;
                // A fixed-flow goal names its role: "<flow>.<role>".
                if goal.rsplit_once('.').map(|(_, suffix)| suffix) != Some(r) {
                    return Err(Error::Denied);
                }
                goal.to_owned()
            }
            _ => return Err(Error::Denied),
        };
        let grant = super::answers::grant_for_root(self.ctx, &self.root)?.ok_or(Error::Denied)?;
        let mut evidence: Vec<String> = match spec.get("evidence_bundle_id") {
            Some(bundle) => {
                owned_result(self.ctx, bundle, "EvidenceBundleRef", &self.root)?.ok_or(Error::Contract)?;
                super::answers::bundle_ids(self.ctx, bundle.as_str().ok_or(Error::Contract)?)?.into_iter().filter(|id| !grant.excluded.contains(id)).collect()
            }
            None if kind == "media" => vec![],
            None => return Err(Error::Contract),
        };
        let mut calculations = vec![];
        for id in spec.get("calculation_ids").and_then(Value::as_array).cloned().unwrap_or_default() {
            owned_result(self.ctx, &id, "CalculationRef", &self.root)?.ok_or(Error::Contract)?;
            calculations.push(id.as_str().unwrap_or("").to_owned());
        }
        let draft = match spec.get("draft_ref") {
            Some(draft) if kind == "check" => {
                let (id, payload) = owned_result(self.ctx, draft, "DraftRef", &self.root)?.ok_or(Error::Contract)?;
                // The checker reads what the draft cited first.
                let cited: Vec<String> = payload["evidence_labels"].as_object().map(|m| m.values().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                let mut ordered = cited.clone();
                ordered.extend(evidence.into_iter().filter(|id| !cited.contains(id)));
                evidence = ordered;
                Some(id)
            }
            None if kind != "check" => None,
            _ => return Err(Error::Contract),
        };
        let mut chars = 0usize;
        let mut kept = vec![];
        let mut omitted = vec![];
        for (index, id) in evidence.into_iter().enumerate() {
            let length: i64 = self.ctx.lock()?.db().query_row("SELECT length(body) FROM chunks WHERE id=?", [&id], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
            if index < PACKET_ITEMS && (kept.len() as i64) < grant.max_items && chars + length as usize <= PACKET_CHARS {
                chars += length as usize;
                kept.push(id);
            } else {
                omitted.push(id);
            }
        }
        let generation = super::indexing::current_generation(self.ctx)?;
        let (packet, refs) = {
            let vault = self.ctx.lock()?;
            let goal = vault.runtime.registry.goal(&goal_id)?.clone();
            let object = |kind: &str, table_kind: &str, text: &str| -> Result<String> {
                let id = vault.register(kind, "m2", Some(owner.id()))?;
                vault.db().execute("INSERT INTO goals(id,root,kind,text,created_at) VALUES(?,?,?,?,?)", params![id, self.root, table_kind, text, now_rfc3339()])?;
                Ok(id)
            };
            let goal_ref = object("GoalRef", "goal", &goal.goal)?;
            let acceptance_ref = object("AcceptanceRef", "acceptance", &goal.acceptance)?;
            let stop_ref = object("StopRef", "stop", &goal.stop)?;
            let mut refs = json!({});
            if kind == "planner" {
                let used: i64 = vault.db().query_row("SELECT generation FROM roots WHERE id=?", [&self.root], |r| r.get(0))?;
                // Keep room for the plan itself, the synthesis, one check and one revision.
                let max_roles = (7 - used - 4).clamp(1, 3);
                let catalog = json!({"roles": ANALYSTS, "max_roles": max_roles});
                refs = json!({"catalog_ref": object("CatalogRef", "catalog", &catalog.to_string())?, "goal_ref": goal_ref, "stop_constraints_ref": object("StopConstraintsRef", "stop", &goal.stop)?});
            }
            let packet = vault.register("ContextPacketRef", "m2", Some(owner.id()))?;
            let payload = json!({"role": role, "agent": agent.id(), "question_id": self.question, "goal": {"goal_id": goal_id, "goal_ref": goal_ref, "acceptance_ref": acceptance_ref, "stop_ref": stop_ref},
                "evidence_bundle_id": spec.get("evidence_bundle_id"), "evidence_ids": kept, "calculation_ids": calculations, "draft_ref": draft, "omitted_ids": omitted,
                "token_estimate": chars * 2 / 3, "source_generation": generation, "created_at": now_rfc3339()});
            vault.db().execute("INSERT INTO context_packets(id,root,agent,role,payload,created_at) VALUES(?,?,?,?,?,?)", params![packet, self.root, agent.id(), role, payload.to_string(), now_rfc3339()])?;
            (packet, refs)
        };
        let mut brief = Value::Null;
        if spec.get("brief").and_then(Value::as_bool) == Some(true) {
            brief = self.brief(&owner, &packet)?;
        }
        Ok(json!({"packet": packet, "refs": refs, "brief": brief, "view": {"items": kept.len(), "omitted": omitted.len(), "tokens": chars * 2 / 3}}))
    }

    /// Context maintenance: a lossy brief of the packet's passages and the
    /// role analyses made so far under the same owner (one request).
    fn brief(&self, owner: &Invocation, packet: &str) -> Result<Value> {
        let (originals, progress) = {
            let vault = self.ctx.lock()?;
            let payload: String = vault.db().query_row("SELECT payload FROM context_packets WHERE id=?", [packet], |r| r.get(0))?;
            let payload: Value = serde_json::from_str(&payload)?;
            let mut originals = vec![];
            for id in payload["evidence_ids"].as_array().cloned().unwrap_or_default().iter().filter_map(Value::as_str) {
                originals.push(vault.register("OriginalRecordRef", "m2", Some(id))?);
            }
            let mut statement = vault.db().prepare("SELECT r.id FROM results r JOIN invocations i ON i.id=r.invocation WHERE r.kind='RoleAnalysisRef' AND i.root=? AND i.parent=?")?;
            let analyses: Vec<String> = statement.query_map(params![self.root, owner.id()], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            let mut progress = vec![];
            for id in analyses {
                progress.push(vault.register("ProgressRef", "m2", Some(&id))?);
            }
            (originals, progress)
        };
        if originals.is_empty() {
            return Err(Error::Contract);
        }
        let input = json!({"context_packet_ref": packet, "progress_refs": progress, "original_record_refs": originals, "target_tokens": 600});
        let invocation = self.ctx.lock()?.runtime.invoke_maintenance(owner, &input)?;
        let envelope = execute(self.ctx, &invocation, &input)?;
        self.collect(&envelope)?;
        if let Some(id) = envelope["data_ref"].as_str() {
            let vault = self.ctx.lock()?;
            let payload: String = vault.db().query_row("SELECT payload FROM context_packets WHERE id=?", [packet], |r| r.get(0))?;
            let mut payload: Value = serde_json::from_str(&payload)?;
            payload["brief_ref"] = json!(id);
            vault.db().execute("UPDATE context_packets SET payload=? WHERE id=?", params![payload.to_string(), packet])?;
        }
        Ok(json!({"envelope": envelope}))
    }

    /// Seal an opened owner: the core builds its result from the references
    /// the flow reports (re-checked) and finalizes the answer it owns.
    fn close(&self, p: &Map<String, Value>) -> Result<Value> {
        let id = p.get("owner").and_then(Value::as_str).ok_or(Error::Contract)?;
        if id == self.base.id() {
            return Err(Error::Denied);
        }
        let owner = self.owner(id)?;
        let output = p.get("output").filter(|v| v.is_object()).ok_or(Error::Contract)?;
        let input: String = self.ctx.lock()?.db().query_row("SELECT input FROM flow_owners WHERE run=? AND invocation=?", params![self.id, id], |r| r.get(0))?;
        let input: Value = serde_json::from_str(&input)?;
        let envelope = match owner.skill() {
            "lvjin-retrieval" => super::workflows::close_retrieval(self.ctx, &owner, &input, output, &self.id)?,
            _ => return Err(Error::Denied),
        };
        self.collect(&envelope)?;
        let view = view_of(self.ctx, &envelope)?;
        Ok(json!({"envelope": envelope, "view": view}))
    }

    fn progress(&self, params: &Value) -> Result<()> {
        let node = params["node"].as_str().unwrap_or("");
        let stage = stage_of(node).ok_or(Error::Worker("protocol_violation".into()))?;
        let status = params["status"].as_str().filter(|s| matches!(*s, "running" | "completed" | "failed")).ok_or(Error::Worker("protocol_violation".into()))?;
        {
            let vault = self.ctx.lock()?;
            let seq: i64 = vault.db().query_row("SELECT COUNT(*) FROM flow_events WHERE run=?", [&self.id], |r| r.get(0))?;
            if seq >= MAX_EVENTS {
                return Ok(());
            }
            vault.db().execute("INSERT INTO flow_events(run,seq,node,status,at) VALUES(?,?,?,?,?)", params![self.id, seq + 1, stage, status, now_rfc3339()])?;
        }
        self.ctx.report(&self.base, stage, 0, 0);
        Ok(())
    }

    fn checkpoint_load(&self, _: &Map<String, Value>) -> Reply {
        let vault = self.ctx.lock()?;
        let rows = |sql: &str, map: &dyn Fn(&rusqlite::Row) -> rusqlite::Result<Value>| -> std::result::Result<Vec<Value>, RpcError> {
            let mut statement = vault.db().prepare(sql)?;
            let collected = statement.query_map([&self.id], |r| map(r))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(collected)
        };
        let parse = |text: String| serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
        let checkpoints = rows("SELECT ns,checkpoint_id,parent_id,checkpoint,metadata FROM flow_checkpoints WHERE run=? ORDER BY created_at,rowid", &|r| {
            Ok(json!({"ns": r.get::<_, String>(0)?, "id": r.get::<_, String>(1)?, "parent": r.get::<_, Option<String>>(2)?, "checkpoint": parse(r.get(3)?), "metadata": parse(r.get(4)?)}))
        })?;
        let blobs = rows("SELECT ns,channel,version,value FROM flow_blobs WHERE run=?", &|r| {
            Ok(json!({"ns": r.get::<_, String>(0)?, "channel": r.get::<_, String>(1)?, "version": parse(r.get(2)?), "value": parse(r.get(3)?)}))
        })?;
        let writes = rows("SELECT ns,checkpoint_id,task_id,idx,channel,value,task_path FROM flow_writes WHERE run=?", &|r| {
            Ok(json!({"ns": r.get::<_, String>(0)?, "checkpoint_id": r.get::<_, String>(1)?, "task_id": r.get::<_, String>(2)?, "idx": r.get::<_, i64>(3)?,
                "channel": r.get::<_, String>(4)?, "value": parse(r.get(5)?), "task_path": r.get::<_, String>(6)?}))
        })?;
        Ok(json!({"checkpoints": checkpoints, "blobs": blobs, "writes": writes}))
    }

    fn charge_storage(&self, vault: &crate::store::Vault, bytes: usize) -> std::result::Result<(), RpcError> {
        let used: i64 = vault.db().query_row("SELECT stored_bytes FROM flow_runs WHERE id=?", [&self.id], |r| r.get(0))?;
        if used + bytes as i64 > MAX_STORED_BYTES {
            return Err(rpc(-32011, "budget"));
        }
        vault.db().execute("UPDATE flow_runs SET stored_bytes=stored_bytes+? WHERE id=?", params![bytes as i64, self.id])?;
        Ok(())
    }

    fn checkpoint_put(&self, p: &Map<String, Value>) -> Reply {
        let ns = p.get("ns").and_then(Value::as_str).filter(|v| v.len() <= 512).ok_or_else(invalid)?;
        let id = text(p, "id", 128)?;
        let parent = match p.get("parent") {
            Some(Value::Null) | None => None,
            Some(Value::String(parent)) if parent.len() <= 128 => Some(parent.clone()),
            _ => return Err(invalid()),
        };
        let (checkpoint, a) = typed(p.get("checkpoint"))?;
        let (metadata, b) = typed(p.get("metadata"))?;
        let blobs = p.get("blobs").and_then(Value::as_array).filter(|b| b.len() <= 64).ok_or_else(invalid)?;
        let mut rows = vec![];
        let mut bytes = a + b;
        for blob in blobs {
            let blob = fields(blob, &["channel", "version", "value"])?;
            let channel = text(blob, "channel", 256)?;
            let version = blob.get("version").filter(|v| v.is_string() || v.is_number()).ok_or_else(invalid)?;
            let (value, size) = typed(blob.get("value"))?;
            bytes += size;
            rows.push((channel.to_owned(), version.to_string(), value));
        }
        let vault = self.ctx.lock()?;
        self.charge_storage(&vault, bytes)?;
        vault.db().execute("INSERT OR REPLACE INTO flow_checkpoints(run,ns,checkpoint_id,parent_id,checkpoint,metadata,created_at) VALUES(?,?,?,?,?,?,?)", params![self.id, ns, id, parent, checkpoint, metadata, now_rfc3339()])?;
        for (channel, version, value) in rows {
            vault.db().execute("INSERT OR REPLACE INTO flow_blobs(run,ns,channel,version,value) VALUES(?,?,?,?,?)", params![self.id, ns, channel, version, value])?;
        }
        Ok(json!({}))
    }

    fn checkpoint_writes(&self, p: &Map<String, Value>) -> Reply {
        let ns = p.get("ns").and_then(Value::as_str).filter(|v| v.len() <= 512).ok_or_else(invalid)?;
        let checkpoint = text(p, "checkpoint_id", 128)?;
        let task = text(p, "task_id", 128)?;
        let path = p.get("task_path").and_then(Value::as_str).filter(|v| v.len() <= 512).ok_or_else(invalid)?;
        let writes = p.get("writes").and_then(Value::as_array).filter(|w| !w.is_empty() && w.len() <= 64).ok_or_else(invalid)?;
        let mut rows = vec![];
        let mut bytes = 0;
        for write in writes {
            let write = fields(write, &["idx", "channel", "value"])?;
            let idx = write.get("idx").and_then(Value::as_i64).filter(|i| (-16..=10_000).contains(i)).ok_or_else(invalid)?;
            let channel = text(write, "channel", 256)?;
            let (value, size) = typed(write.get("value"))?;
            bytes += size;
            rows.push((idx, channel.to_owned(), value));
        }
        let vault = self.ctx.lock()?;
        self.charge_storage(&vault, bytes)?;
        for (idx, channel, value) in rows {
            vault.db().execute("INSERT OR REPLACE INTO flow_writes(run,ns,checkpoint_id,task_id,idx,channel,value,task_path) VALUES(?,?,?,?,?,?,?,?)", params![self.id, ns, checkpoint, task, idx, channel, value, path])?;
        }
        Ok(json!({}))
    }
}

/// Progress nodes map to fixed stage names (UI labels); others are rejected.
fn stage_of(node: &str) -> Option<&'static str> {
    Some(match node {
        "plan" => "plan",
        "search" => "search",
        "rerank" => "rerank",
        "draft" => "draft",
        "revise" => "revise",
        "verify" => "verify",
        "plan_tasks" => "plan_tasks",
        "role" => "role",
        "check" => "check",
        "synthesize" => "synthesize",
        "observe" => "observe",
        "calculate" => "calculate",
        _ => return None,
    })
}

/// What the orchestrator may see of a step's result: kinds, references,
/// counts and the planned queries — never document or answer text.
pub(crate) fn view_of(ctx: &Ctx, envelope: &Value) -> Result<Value> {
    let Some(id) = envelope["data_ref"].as_str() else { return Ok(json!({})) };
    let (_, kind, payload) = ctx.lock()?.get_result(id)?;
    let count = |value: &Value| value.as_array().map(Vec::len).unwrap_or(0);
    let status_count = |status: &str| payload["claims"].as_array().map(|c| c.iter().filter(|r| r["status"] == status).count()).unwrap_or(0);
    Ok(match kind.as_str() {
        "SearchPlanRef" => json!({"kind": kind, "rewritten_queries": payload["plan"]["rewritten_queries"]}),
        "CandidateSetRef" => json!({"kind": kind, "candidates": count(&payload["candidates"])}),
        "EvidenceBundleRef" => json!({"kind": kind, "evidence_ids": envelope["evidence_refs"], "initial_take": payload["initial_take"]}),
        "EvidenceReadRef" => json!({"kind": kind, "items": count(&payload["items"])}),
        "DraftRef" => json!({"kind": kind, "version": payload["version"], "claims": count(&payload["draft"]["claims"]), "insufficient_evidence": payload["insufficient_evidence"]}),
        "ValidationReportRef" => json!({"kind": kind,
            "required_actions": payload["required_actions"].as_array().cloned().unwrap_or_default().iter().map(|a| json!({"claim_id": a["claim_id"], "reason": a["reason"]})).collect::<Vec<_>>(),
            "supported": status_count("supported"), "unsupported": status_count("unsupported"), "unchecked": status_count("unchecked")}),
        "TaskPlanRef" => json!({"kind": kind, "task_graph": payload["plan"]["task_graph"]}),
        "RoleAnalysisRef" => json!({"kind": kind, "role": payload["role"], "claims": count(&payload["analysis"]["claims"]), "proposed_skill_ids": payload["analysis"]["proposed_skill_ids"],
            "invalid_citations": count(&payload["invalid_citations"])}),
        "ChallengeReportRef" => json!({"kind": kind,
            "challenges": payload["report"]["challenges"].as_array().cloned().unwrap_or_default().iter().map(|c| json!({"claim_id": c["claim_id"], "issue_type": c["issue_type"]})).collect::<Vec<_>>()}),
        "ContextBriefRef" => json!({"kind": kind, "lossy": true}),
        "MediaObservationRef" => json!({"kind": kind, "observations": count(&payload["observation"]["observations"]), "disagreements": count(&payload["comparison"]["disagreements"]),
            "unreadable": count(&payload["comparison"]["unreadable"])}),
        "RetrievalResultRef" => json!({"kind": kind, "answer": !payload["answer"].is_null(), "evidence_bundle": payload["evidence_bundle"]}),
        _ => json!({"kind": kind}),
    })
}

/// A result object of `kind` produced by an invocation of `root`.
pub(crate) fn owned_result(ctx: &Ctx, id: &Value, kind: &str, root: &str) -> Result<Option<(String, Value)>> {
    let Some(id) = id.as_str() else { return Ok(None) };
    let vault = ctx.lock()?;
    let owner: Option<String> = vault.db().query_row("SELECT i.root FROM results r JOIN invocations i ON i.id=r.invocation WHERE r.id=? AND r.kind=?", params![id, kind], |r| r.get(0)).optional()?;
    if owner.as_deref() != Some(root) {
        return Err(Error::Denied);
    }
    Ok(Some((id.to_owned(), vault.get_result(id)?.2)))
}

/// Process steps reported by the flow, reduced to known fields and bounds.
pub(crate) fn clean_process(value: &Value) -> Vec<Value> {
    const NUMBERS: &[&str] = &["ms", "rewrites", "queries", "candidates", "evidence", "supported", "problems", "roles", "challenges", "observations"];
    let mut out = vec![];
    for row in value.as_array().map(|rows| rows.iter().take(32).collect::<Vec<_>>()).unwrap_or_default() {
        let (Some(step), Some(status)) = (row["step"].as_str().and_then(stage_of), row["status"].as_str()) else { continue };
        let status = crate::sanitize_code(status);
        let mut clean = json!({"step": step, "status": status});
        for key in NUMBERS {
            if let Some(number) = row[*key].as_u64() {
                clean[*key] = json!(number.min(10_000_000));
            }
        }
        if let Some(code) = row["code"].as_str() {
            clean["code"] = json!(crate::sanitize_code(code));
        }
        if let Some(role) = row["role"].as_str().filter(|r| r.len() <= 40 && r.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')) {
            clean["role"] = json!(role);
        }
        out.push(clean);
    }
    out
}

/// Messages for the codes a flow itself reports (step outcomes it decided).
pub(crate) fn flow_warning(code: &str) -> Option<&'static str> {
    Some(match code {
        "plan_fallback" => "查询规划未完成，改用原始问题检索。",
        "revision_stopped" => "修订未完成，保留上一版回答。",
        "insufficient_evidence" => "依据不足：所选范围内没有检索到可用于回答的原文，未生成回答。",
        "role_incomplete" => "有角色分析未完成（额度不足或请求失败），结论只基于已完成的部分。",
        "check_skipped" => "检查者未能完成复核（额度不足或请求失败）；回答仍经过逐句核查。",
        "native_media_not_sent" => "没有直接读取原始媒体（接口未通过对应能力自检、未获准或请求失败）；按本机识别文字分析。",
        _ => return None,
    })
}
