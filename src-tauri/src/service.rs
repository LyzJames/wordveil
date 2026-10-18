//! The trusted core service used by the desktop shell, the validation tool and
//! the tests. It owns the unlocked vault, issues roots for real user actions,
//! runs skills on worker threads and exposes read-only views. Secrets
//! (passphrases, recovery codes) arrive only through dedicated methods and
//! never become Skill input.
use crate::{
    Error, Result,
    crypto::KdfParams,
    handlers::{self, Ctx, Progress, VaultHandle},
    runtime::{Registry, UserAction},
    services::{Layout, Services},
    store::{Vault, sha256_hex},
    vault::{self, VaultSummary, now_rfc3339},
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

/// Capabilities any user action may receive (local only). Generation and
/// probing are conditional and granted only to confirmed/protected roots.
pub const LOCAL_CAPABILITIES: &[&str] = &[
    "vault.read", "vault.write", "file.selection.read", "local.ocr", "local.embedding", "local.rerank", "local.asr", "local.calc",
    "index.read", "index.write", "graph.read", "media.decode", "diagnostics.redacted", "local.grounding",
];
const PROTECTED_CAPABILITIES: &[&str] = &["vault.maintenance", "settings.privacy", "settings.connection"];
/// Conditional capabilities. They are granted only to roots created by an
/// explicit confirmation or a protected flow, so a plain UI or agent root can
/// read and preview but never commit, export, back up, store a key, probe an
/// endpoint or send anything to a generation service. Generation additionally
/// needs an approved, destination-bound egress grant bound to the root.
const CONFIRMED_CAPABILITIES: &[&str] = &["vault.write", "graph.write", "file.export", "file.backup", "credential.write", "network.probe", "generation.request"];
/// Bounds of one approved answer task (design 7.5; root limit 7 requests).
const EGRESS_MAX_ITEMS: i64 = 10;
const EGRESS_MAX_REQUESTS: i64 = 7;
const TOKEN_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, serde::Serialize)]
pub struct TaskEvent {
    pub task_id: String,
    pub kind: &'static str,
    pub progress: Option<Progress>,
    pub envelope: Option<Value>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct TaskState {
    pub task_id: String,
    pub skill: String,
    pub root: String,
    pub status: String,
    pub progress: Option<Progress>,
    pub envelope: Option<Value>,
    pub started_at: String,
}

struct Task {
    state: TaskState,
    cancel: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct Session {
    handle_name: String,
    vault: VaultHandle,
}

pub struct Config {
    pub kdf: KdfParams,
    pub auto_lock: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self { kdf: KdfParams::DESIGN_DEFAULT, auto_lock: Duration::from_secs(15 * 60) }
    }
}

type Sink = Box<dyn Fn(TaskEvent) + Send + Sync>;

pub struct CoreService {
    pub services: Arc<Services>,
    config: Config,
    session: Mutex<Option<Session>>,
    tasks: Arc<Mutex<HashMap<String, Task>>>,
    tokens: Mutex<HashMap<String, (String, String, Instant)>>,
    sink: Arc<Sink>,
    activity: Mutex<Instant>,
    failures: Mutex<(u32, Option<Instant>)>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct UnlockInfo {
    pub handle: String,
    pub vault_id: String,
    pub root_matter: String,
    pub name: String,
}

pub enum Secret {
    Passphrase(Zeroizing<String>),
    Recovery(Zeroizing<String>),
}

impl CoreService {
    pub fn new(layout: Layout, config: Config, sink: Sink) -> Result<Arc<Self>> {
        let services = Arc::new(Services::new(layout)?);
        services.log.event("core.started", &[("production", crate::logging::Field::Flag(services.layout.production))]);
        Ok(Arc::new(Self {
            services,
            config,
            session: Mutex::new(None),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            tokens: Mutex::new(HashMap::new()),
            sink: Arc::new(sink),
            activity: Mutex::new(Instant::now()),
            failures: Mutex::new((0, None)),
        }))
    }

    pub fn touch(&self) {
        if let Ok(mut last) = self.activity.lock() {
            *last = Instant::now();
        }
    }

    fn vault(&self) -> Result<VaultHandle> {
        self.touch();
        self.session.lock().map_err(|_| Error::Integrity)?.as_ref().map(|s| s.vault.clone()).ok_or(Error::Locked)
    }

    pub fn is_unlocked(&self) -> bool {
        self.session.lock().map(|s| s.is_some()).unwrap_or(false)
    }

    pub fn list_vaults(&self) -> Result<Vec<VaultSummary>> {
        vault::list(&self.services.layout.vaults_root())
    }

    fn registry(&self) -> Result<Registry> {
        Registry::load(&self.services.layout.resources, false)
    }

    fn throttle(&self) -> Result<()> {
        let failures = self.failures.lock().map_err(|_| Error::Integrity)?;
        if let (count, Some(at)) = *failures {
            if count >= 5 && at.elapsed() < Duration::from_secs(30) {
                return Err(Error::Worker("too_many_attempts".into()));
            }
        }
        Ok(())
    }

    fn record_failure(&self, failed: bool) {
        if let Ok(mut failures) = self.failures.lock() {
            *failures = if failed { (failures.0 + 1, Some(Instant::now())) } else { (0, None) };
        }
    }

    /// Protected UI flow: create a vault. Returns the recovery code exactly once.
    pub fn create_vault(&self, passphrase: Zeroizing<String>, name: &str) -> Result<(UnlockInfo, Zeroizing<String>)> {
        if self.is_unlocked() {
            return Err(Error::Conflict);
        }
        if passphrase.chars().count() < 8 {
            return Err(Error::NeedsInput("passphrase_too_short"));
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            return Err(Error::NeedsInput("vault_name"));
        }
        let handle = format!("{:x}", uuid::Uuid::new_v4().simple());
        let directory = vault::directory_for(&self.services.layout.vaults_root(), &handle)?;
        let started = Instant::now();
        let (header, keys, recovery) = vault::create(&directory, passphrase.as_bytes(), self.config.kdf)?;
        let opened = Vault::open(&directory, header, keys, self.registry()?)?;
        let root_matter = opened.root_matter()?;
        opened.set_setting("vault_name", name)?;
        opened.set_setting("auto_lock_minutes", &(self.config.auto_lock.as_secs() / 60).max(1).to_string())?;
        let info = UnlockInfo { handle: handle.clone(), vault_id: opened.id.clone(), root_matter, name: name.into() };
        self.install_session(handle, opened)?;
        self.protected_audit("create")?;
        self.services.log.event("vault.created", &[("kdf_ms", crate::logging::Field::Millis(started.elapsed().as_millis() as u64))]);
        Ok((info, recovery.0))
    }

    /// Protected UI flow: unlock with the passphrase or the recovery code.
    pub fn unlock(&self, handle: &str, secret: Secret) -> Result<UnlockInfo> {
        if self.is_unlocked() {
            return Err(Error::Conflict);
        }
        self.throttle()?;
        let directory = vault::directory_for(&self.services.layout.vaults_root(), handle)?;
        let header = vault::read_header(&directory)?;
        let started = Instant::now();
        let keys = match &secret {
            Secret::Passphrase(text) => vault::unlock_with_passphrase(&header, text.as_bytes()),
            Secret::Recovery(code) => vault::unlock_with_recovery(&header, code),
        };
        let keys = match keys {
            Ok(keys) => keys,
            Err(error) => {
                self.record_failure(true);
                self.services.log.event("vault.unlock_failed", &[]);
                return Err(error);
            }
        };
        self.record_failure(false);
        let opened = Vault::open(&directory, header, keys, self.registry()?)?;
        let root_matter = opened.root_matter()?;
        let name = opened.setting("vault_name")?.unwrap_or_else(|| "私密空间".into());
        let info = UnlockInfo { handle: handle.into(), vault_id: opened.id.clone(), root_matter, name };
        // Imports interrupted by a crash are marked for resumption, never duplicated.
        opened.db().execute("UPDATE jobs SET status='interrupted' WHERE status='running'", [])?;
        opened.db().execute("UPDATE document_versions SET status='interrupted' WHERE status IN ('parsing','ocr','indexing','stored')", [])?;
        self.install_session(handle.into(), opened)?;
        self.protected_audit("unlock")?;
        self.services.log.event("vault.unlocked", &[("kdf_ms", crate::logging::Field::Millis(started.elapsed().as_millis() as u64)), ("recovery", crate::logging::Field::Flag(matches!(secret, Secret::Recovery(_))))]);
        Ok(info)
    }

    fn install_session(&self, handle_name: String, opened: Vault) -> Result<()> {
        let mut session = self.session.lock().map_err(|_| Error::Integrity)?;
        *session = Some(Session { handle_name, vault: Arc::new(Mutex::new(opened)) });
        self.touch();
        Ok(())
    }

    /// Record a protected-UI action through privacy-settings for the audit trail.
    fn protected_audit(&self, operation: &str) -> Result<()> {
        let request = self.ui_request(operation, &json!({}))?;
        self.run_skill("privacy-settings", json!({"operation": operation, "ui_request_id": request}), "protected")?;
        Ok(())
    }

    /// Lock: stop tasks, drop keys and sensitive workers, forget view tokens.
    pub fn lock(&self) -> Result<()> {
        let ids: Vec<String> = self.tasks.lock().map_err(|_| Error::Integrity)?.keys().cloned().collect();
        for id in &ids {
            let _ = self.cancel_task(id);
        }
        let threads: Vec<std::thread::JoinHandle<()>> = {
            let mut tasks = self.tasks.lock().map_err(|_| Error::Integrity)?;
            tasks.values_mut().filter_map(|t| t.thread.take()).collect()
        };
        for thread in threads {
            let _ = thread.join();
        }
        if self.is_unlocked() {
            let _ = self.protected_audit("lock");
        }
        self.session.lock().map_err(|_| Error::Integrity)?.take();
        self.tokens.lock().map_err(|_| Error::Integrity)?.clear();
        self.tasks.lock().map_err(|_| Error::Integrity)?.clear();
        self.services.stop_sensitive();
        self.services.log.event("vault.locked", &[]);
        Ok(())
    }

    /// Called periodically by the shell; locks after the configured idle time.
    pub fn auto_lock_tick(&self) -> Result<bool> {
        let Ok(handle) = self.vault() else { return Ok(false) };
        let minutes: u64 = handle.lock().map_err(|_| Error::Integrity)?.setting("auto_lock_minutes")?.and_then(|v| v.parse().ok()).unwrap_or(15);
        let busy = self.tasks.lock().map_err(|_| Error::Integrity)?.values().any(|t| t.state.status == "running");
        let idle = self.activity.lock().map_err(|_| Error::Integrity)?.elapsed();
        if !busy && idle >= Duration::from_secs(minutes * 60) {
            self.lock()?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn change_passphrase(&self, current: Zeroizing<String>, new: Zeroizing<String>) -> Result<()> {
        if new.chars().count() < 8 {
            return Err(Error::NeedsInput("passphrase_too_short"));
        }
        let handle = self.vault()?;
        {
            let mut vault = handle.lock().map_err(|_| Error::Integrity)?;
            vault::unlock_with_passphrase(&vault.header, current.as_bytes()).map_err(|_| Error::Denied)?;
            let updated = vault::rewrap_passphrase(&vault.dir.clone(), &vault.header.clone(), &vault.keys, new.as_bytes(), self.config.kdf)?;
            vault.header = updated;
        }
        let request = self.ui_request("change_policy", &json!({"passphrase_changed": true}))?;
        self.run_skill("privacy-settings", json!({"operation": "change_policy", "ui_request_id": request}), "protected")?;
        Ok(())
    }

    /// Restore a backup into a new vault directory before any vault is unlocked
    /// (simulated device change: only the backup and its passphrase/code).
    pub fn restore_backup(&self, backup_root: &Path, secret: Secret) -> Result<String> {
        self.throttle()?;
        let package = handlers::maintenance::read_backup(backup_root)?;
        let keys = match &secret {
            Secret::Passphrase(text) => vault::unlock_with_passphrase(&package.header, text.as_bytes()),
            Secret::Recovery(code) => vault::unlock_with_recovery(&package.header, code),
        };
        let keys = keys.inspect_err(|_| self.record_failure(true))?;
        self.record_failure(false);
        let handle = handlers::maintenance::restore(&package, &keys, &self.services.layout.vaults_root())?;
        self.services.log.event("vault.restored", &[]);
        Ok(handle)
    }

    // ------------------------------------------------------------ trusted objects

    fn ui_request(&self, operation: &str, payload: &Value) -> Result<String> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let id = vault.register("TrustedUiRequestRef", "m1", None)?;
        vault.db().execute("INSERT INTO ui_requests(id,operation,payload,created_at) VALUES(?,?,?,?)", params![id, operation, payload.to_string(), now_rfc3339()])?;
        Ok(id)
    }

    pub fn privacy_request(&self, operation: &str, payload: &Value) -> Result<String> {
        if !matches!(operation, "view_grants" | "revoke_grant" | "change_policy") {
            return Err(Error::Contract);
        }
        self.ui_request(operation, payload)
    }

    /// A file picked in the native dialog becomes a scoped selection object.
    pub fn register_selection(&self, kind: &str, purpose: &str, path: &Path) -> Result<Value> {
        let allowed = matches!((kind, purpose), ("SelectionRef", "import") | ("OutputSelectionRef", "export") | ("MaintenanceSelectionRef", "backup_destination") | ("MaintenanceSelectionRef", "backup_source"));
        if !allowed {
            return Err(Error::Contract);
        }
        let display = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let (bytes, digest) = if kind == "SelectionRef" {
            let metadata = std::fs::metadata(path)?;
            if !metadata.is_file() {
                return Err(Error::Contract);
            }
            let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
            let mut file = std::io::BufReader::new(std::fs::File::open(path)?);
            std::io::copy(&mut file, &mut HashWriter(&mut hasher))?;
            (Some(metadata.len() as i64), Some(format!("{:x}", sha2::Digest::finalize(hasher))))
        } else {
            (None, None)
        };
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let id = vault.register(kind, "m1", None)?;
        vault.db().execute(
            "INSERT INTO selections(id,kind,path,display_name,bytes,sha256,purpose,created_at) VALUES(?,?,?,?,?,?,?,?)",
            params![id, kind, path.to_string_lossy(), display, bytes, digest, purpose, now_rfc3339()],
        )?;
        Ok(json!({"selection_id": id, "display_name": display, "bytes": bytes}))
    }

    pub fn create_question(&self, text: &str) -> Result<String> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 2000 {
            return Err(Error::NeedsInput("question"));
        }
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let id = vault.register("ImmutableQuestionRef", "m1", None)?;
        vault.db().execute("INSERT INTO questions(id,text,created_at) VALUES(?,?,?)", params![id, text, now_rfc3339()])?;
        Ok(id)
    }

    pub fn create_scope(&self, matters: &[String], versions: &[String]) -> Result<String> {
        let handle = self.vault()?;
        let ctx_cancel = AtomicBool::new(false);
        let sink = |_: Progress| {};
        let ctx = Ctx { vault: &handle, services: &self.services, cancel: &ctx_cancel, progress: &sink };
        handlers::workflows::create_scope(&ctx, matters, versions)
    }

    pub fn create_range(&self, media: &str, start_s: f64, end_s: Option<f64>) -> Result<String> {
        if !(start_s >= 0.0) || end_s.is_some_and(|e| e <= start_s) {
            return Err(Error::Contract);
        }
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.expect_kind(media, &["MediaVersionRef"])?;
        let id = vault.register("ApprovedTimeRangeRef", "m1", Some(media))?;
        vault.db().execute("INSERT INTO ranges(id,media_id,start_s,end_s,created_at) VALUES(?,?,?,?,?)", params![id, media, start_s, end_s, now_rfc3339()])?;
        Ok(id)
    }

    pub fn create_filter(&self, kind: &str, ids: &[String]) -> Result<String> {
        if !matches!(kind, "matter" | "version") || ids.is_empty() {
            return Err(Error::Contract);
        }
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        for id in ids {
            vault.expect_kind(id, if kind == "matter" { &["MatterRef"] } else { &["DocumentVersionRef"] })?;
        }
        let filter = vault.register("KnownFilterRef", "m1", None)?;
        vault.db().execute("INSERT INTO filters(id,kind,value) VALUES(?,?,?)", params![filter, kind, serde_json::to_string(ids)?])?;
        Ok(filter)
    }

    /// Draft a matter change; matter-manage validates/prepares and commits it.
    pub fn draft_change(&self, matter: &str, change: &Value) -> Result<String> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.expect_kind(matter, &["MatterRef"])?;
        if change.to_string().len() > 2 * 1024 * 1024 || !change["type"].is_string() {
            return Err(Error::Contract);
        }
        let id = vault.register("PreparedChangeRef", "m1", Some(matter))?;
        vault.db().execute("INSERT INTO changes(id,matter_id,change,impact,status,created_at) VALUES(?,?,?,'{}','draft',?)", params![id, matter, change.to_string(), now_rfc3339()])?;
        Ok(id)
    }

    pub fn component_ref(&self, name: &str) -> Result<String> {
        if !matches!(name, "installation" | "model_pool" | "local_worker" | "index" | "storage") {
            return Err(Error::Contract);
        }
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let key = format!("component:{name}");
        if let Some(id) = vault.setting(&key)? {
            return Ok(id);
        }
        let id = vault.register("ComponentRef", "m1", None)?;
        vault.set_setting(&key, &id)?;
        Ok(id)
    }

    // ---------------------------------------------------------------- execution

    fn capabilities(&self, skill: &str, action: &str, registry: &Registry) -> Vec<&'static str> {
        fn walk(registry: &Registry, id: &str, conditional: bool, out: &mut std::collections::BTreeSet<String>) {
            if let Some(skill) = registry.skills.get(id) {
                out.extend(if conditional { skill.conditional_capabilities.iter() } else { skill.direct_capabilities.iter() }.cloned());
                for child in &skill.children {
                    walk(registry, child, conditional, out);
                }
            }
        }
        let mut direct = std::collections::BTreeSet::new();
        walk(registry, skill, false, &mut direct);
        let mut granted: Vec<&'static str> = LOCAL_CAPABILITIES.iter().copied().filter(|c| direct.contains(*c)).collect();
        if action == "protected" || action == "confirm" {
            let mut conditional = std::collections::BTreeSet::new();
            walk(registry, skill, true, &mut conditional);
            granted.extend(PROTECTED_CAPABILITIES.iter().copied().filter(|c| direct.contains(*c)));
            for capability in CONFIRMED_CAPABILITIES {
                if conditional.contains(*capability) && !granted.contains(capability) {
                    granted.push(capability);
                }
            }
        }
        granted
    }

    fn begin(&self, skill: &str, input: &Value, action: &str) -> Result<(VaultHandle, crate::runtime::Invocation)> {
        self.begin_with(skill, input, action, None)
    }

    /// `grant`: an approved egress grant to bind to the new root before the
    /// first invocation runs (one grant, one root, never reused).
    fn begin_with(&self, skill: &str, input: &Value, action: &str, grant: Option<&str>) -> Result<(VaultHandle, crate::runtime::Invocation)> {
        if !matches!(action, "ui" | "confirm" | "protected") {
            return Err(Error::Contract);
        }
        let handle = self.vault()?;
        let invocation = {
            let mut vault = handle.lock().map_err(|_| Error::Integrity)?;
            let scope = vault.id.clone();
            let capabilities = self.capabilities(skill, action, &vault.runtime.registry);
            let root = vault.runtime.begin_root(UserAction::trusted(&scope, &capabilities, action))?;
            if let Some(grant) = grant {
                let bound = vault.db().execute("UPDATE egress_grants SET root=? WHERE id=? AND root IS NULL AND approved_at IS NOT NULL AND revoked_at IS NULL", params![root.id(), grant])?;
                if bound != 1 {
                    return Err(Error::Denied);
                }
            }
            vault.runtime.invoke_ui(&root, skill, input)?
        };
        Ok((handle, invocation))
    }

    /// Run a skill to completion on the calling thread (tests, validation tool).
    pub fn run_skill(&self, skill: &str, input: Value, action: &str) -> Result<Value> {
        let (handle, invocation) = self.begin(skill, &input, action)?;
        let cancel = AtomicBool::new(false);
        let sink = |_: Progress| {};
        let ctx = Ctx { vault: &handle, services: &self.services, cancel: &cancel, progress: &sink };
        handlers::execute(&ctx, &invocation, &input)
    }

    /// Start a skill on a worker thread; progress/finish events go to the sink.
    pub fn start_skill(self: &Arc<Self>, skill: &str, input: Value, action: &str) -> Result<String> {
        let (handle, invocation) = self.begin(skill, &input, action)?;
        self.spawn(skill, input, handle, invocation)
    }

    fn spawn(self: &Arc<Self>, skill: &str, input: Value, handle: VaultHandle, invocation: crate::runtime::Invocation) -> Result<String> {
        let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());
        let cancel = Arc::new(AtomicBool::new(false));
        let state = TaskState { task_id: task_id.clone(), skill: skill.into(), root: invocation.root_id().into(), status: "running".into(), progress: None, envelope: None, started_at: now_rfc3339() };
        self.tasks.lock().map_err(|_| Error::Integrity)?.insert(task_id.clone(), Task { state, cancel: cancel.clone(), thread: None });
        let service = self.clone();
        let id = task_id.clone();
        let thread = std::thread::spawn(move || {
            let tasks = service.tasks.clone();
            let sink = service.sink.clone();
            let progress_id = id.clone();
            let on_progress = move |progress: Progress| {
                if let Ok(mut tasks) = tasks.lock() {
                    if let Some(task) = tasks.get_mut(&progress_id) {
                        task.state.progress = Some(progress.clone());
                    }
                }
                sink(TaskEvent { task_id: progress_id.clone(), kind: "progress", progress: Some(progress), envelope: None });
            };
            let ctx = Ctx { vault: &handle, services: &service.services, cancel: &cancel, progress: &on_progress };
            // A cancelled root can no longer seal an envelope; report it as cancelled.
            let envelope = handlers::execute(&ctx, &invocation, &input).unwrap_or_else(|error| match error {
                Error::Cancelled => json!({"status": "cancelled", "warnings": [{"code": "cancelled", "message": "任务已停止、撤销或空间已锁定。"}]}),
                other => json!({"status": "failed", "warnings": [{"code": other.code(), "message": "执行失败"}]}),
            });
            if let Ok(mut tasks) = service.tasks.lock() {
                if let Some(task) = tasks.get_mut(&id) {
                    task.state.status = envelope["status"].as_str().unwrap_or("failed").into();
                    task.state.envelope = Some(envelope.clone());
                }
            }
            (service.sink)(TaskEvent { task_id: id.clone(), kind: "finished", progress: None, envelope: Some(envelope) });
        });
        if let Some(task) = self.tasks.lock().map_err(|_| Error::Integrity)?.get_mut(&task_id) {
            task.thread = Some(thread);
        }
        Ok(task_id)
    }

    pub fn task(&self, id: &str) -> Result<TaskState> {
        self.tasks.lock().map_err(|_| Error::Integrity)?.get(id).map(|t| t.state.clone()).ok_or(Error::Denied)
    }

    pub fn tasks(&self) -> Result<Vec<TaskState>> {
        let mut list: Vec<TaskState> = self.tasks.lock().map_err(|_| Error::Integrity)?.values().map(|t| t.state.clone()).collect();
        list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(list)
    }

    /// Cancel a running task: flag workers and cancel the ledger root so any
    /// later dispatch or commit is rejected (cancellation propagates).
    pub fn cancel_task(&self, id: &str) -> Result<()> {
        let root = {
            let tasks = self.tasks.lock().map_err(|_| Error::Integrity)?;
            let task = tasks.get(id).ok_or(Error::Denied)?;
            task.cancel.store(true, Ordering::Release);
            task.state.root.clone()
        };
        if let Ok(handle) = self.vault() {
            handle.lock().map_err(|_| Error::Integrity)?.runtime.cancel_root(&root)?;
        }
        Ok(())
    }

    pub fn wait_task(&self, id: &str, timeout: Duration) -> Result<TaskState> {
        let deadline = Instant::now() + timeout;
        loop {
            let state = self.task(id)?;
            if state.status != "running" {
                return Ok(state);
            }
            if Instant::now() >= deadline {
                return Err(Error::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // ------------------------------------------------------------- read views

    pub fn result(&self, id: &str) -> Result<Value> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let (skill, kind, payload) = vault.get_result(id)?;
        Ok(json!({"id": id, "skill": skill, "kind": kind, "payload": payload}))
    }

    fn issue_token(&self, blob: &str, mime: &str) -> Result<String> {
        let token = format!("{:x}{:x}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let mut tokens = self.tokens.lock().map_err(|_| Error::Integrity)?;
        tokens.retain(|_, (_, _, at)| at.elapsed() < TOKEN_TTL);
        tokens.insert(token.clone(), (blob.into(), mime.into(), Instant::now()));
        Ok(token)
    }

    /// Read-only page view (no processing): available image token, text blocks
    /// with locators, and the chunks on the page for an evidence-read render.
    pub fn page_view(&self, page: &str) -> Result<Value> {
        let handle = self.vault()?;
        let (image, row, blocks, chunks) = {
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            vault.expect_kind(page, &["PageRef"])?;
            let row: (String, i64, String, Option<f64>, Option<f64>, Option<String>, i64, i64) = vault.db().query_row(
                "SELECT version_id,page_no,source,width,height,image_blob,rotation_turns,uvdoc FROM pages WHERE id=?",
                [page],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
            )?;
            let mut statement = vault.db().prepare("SELECT rowid,text,locator,confidence,kind FROM blocks WHERE page_id=? ORDER BY ordinal")?;
            let blocks: Vec<Value> = statement
                .query_map([page], |r| Ok(json!({"block": r.get::<_, i64>(0)?, "text": r.get::<_, String>(1)?, "locator": serde_json::from_str::<Value>(&r.get::<_, String>(2)?).unwrap_or(Value::Null), "confidence": r.get::<_, Option<f64>>(3)?, "kind": r.get::<_, String>(4)?})))?
                .collect::<std::result::Result<_, _>>()?;
            let block_ids: Vec<i64> = blocks.iter().filter_map(|b| b["block"].as_i64()).collect();
            let mut chunk_statement = vault.db().prepare("SELECT c.id,c.spans FROM chunks c JOIN objects o ON o.id=c.id WHERE c.version_id=? AND o.active=1 ORDER BY c.ordinal")?;
            let chunks: Vec<String> = chunk_statement
                .query_map([&row.0], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .filter_map(|r| r.ok())
                .filter(|(_, spans)| serde_json::from_str::<Vec<Value>>(spans).unwrap_or_default().iter().any(|s| s["block"].as_i64().is_some_and(|b| block_ids.contains(&b))))
                .map(|(id, _)| id)
                .collect();
            (row.5.clone(), row, blocks, chunks)
        };
        let token = match image {
            Some(blob) => Some(self.issue_token(&blob, "image/png")?),
            None => None,
        };
        Ok(json!({"page_id": page, "version_id": row.0, "page_no": row.1, "source": row.2, "width": row.3, "height": row.4, "image_token": token,
            "rotation_turns": row.6, "unwarped": row.7 != 0, "blocks": blocks, "chunks": chunks}))
    }

    /// Pages of a document version (ids and status only).
    pub fn version_pages(&self, version: &str) -> Result<Value> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.expect_kind(version, &["DocumentVersionRef"])?;
        let mut statement = vault.db().prepare("SELECT id,page_no,source,ocr_status FROM pages WHERE version_id=? ORDER BY page_no")?;
        let pages: Vec<Value> = statement
            .query_map([version], |r| Ok(json!({"page_id": r.get::<_, String>(0)?, "page_no": r.get::<_, i64>(1)?, "source": r.get::<_, String>(2)?, "ocr_status": r.get::<_, String>(3)?})))?
            .collect::<std::result::Result<_, _>>()?;
        let mut blocks = vault.db().prepare("SELECT rowid,kind,text,locator FROM blocks WHERE version_id=? AND page_id IS NULL ORDER BY ordinal LIMIT 5000")?;
        let free: Vec<Value> = blocks
            .query_map([version], |r| Ok(json!({"block": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?, "text": r.get::<_, String>(2)?, "locator": serde_json::from_str::<Value>(&r.get::<_, String>(3)?).unwrap_or(Value::Null)})))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(json!({"version_id": version, "pages": pages, "blocks": free}))
    }

    /// Playable WAV for one decoded ASR segment (format conversion only).
    pub fn segment_audio(&self, segment: &str) -> Result<Value> {
        let handle = self.vault()?;
        let blob = {
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            vault.expect_kind(segment, &["AudioSegmentRef"])?;
            vault.db().query_row("SELECT pcm_blob FROM audio_segments WHERE id=?", [segment], |r| r.get::<_, String>(0))?
        };
        Ok(json!({"segment_id": segment, "audio_token": self.issue_token(&format!("wav:{blob}"), "audio/wav")?}))
    }

    /// Serve a token for the custom protocol. Tokens die on lock or expiry.
    pub fn serve_token(&self, token: &str) -> Result<(Vec<u8>, String)> {
        let (blob, mime) = {
            let tokens = self.tokens.lock().map_err(|_| Error::Integrity)?;
            let (blob, mime, at) = tokens.get(token).ok_or(Error::Denied)?;
            if at.elapsed() >= TOKEN_TTL {
                return Err(Error::Denied);
            }
            (blob.clone(), mime.clone())
        };
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        match blob.strip_prefix("wav:") {
            Some(pcm) => {
                let samples = vault.get_blob(pcm)?;
                Ok((wav_from_f32(&samples), mime))
            }
            None => Ok((vault.get_blob(&blob)?.to_vec(), mime)),
        }
    }

    pub fn session_handle(&self) -> Option<String> {
        self.session.lock().ok().and_then(|s| s.as_ref().map(|s| s.handle_name.clone()))
    }

    pub fn data_root(&self) -> PathBuf {
        self.services.layout.data_root.clone()
    }

    pub fn fingerprint(text: &str) -> String {
        sha256_hex(text.as_bytes())[..12].to_owned()
    }

    pub fn has_interrupted_imports(&self) -> Result<Vec<String>> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let mut statement = vault.db().prepare("SELECT json_extract(state,'$.selection') FROM jobs WHERE status='interrupted'")?;
        let rows = statement.query_map([], |r| r.get::<_, Option<String>>(0))?;
        Ok(rows.filter_map(|r| r.ok().flatten()).collect())
    }

    /// Interrupted imports with the matter and file name needed to resume them.
    pub fn interrupted_imports(&self) -> Result<Vec<Value>> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let mut statement = vault.db().prepare(
            "SELECT json_extract(j.state,'$.selection'),j.id,d.matter_id,d.display_name,j.updated_at FROM jobs j JOIN document_versions v ON v.id=j.id JOIN documents d ON d.id=v.document_id WHERE j.status='interrupted' ORDER BY j.updated_at DESC",
        )?;
        let rows = statement.query_map([], |r| Ok(json!({"selection_id": r.get::<_, Option<String>>(0)?, "version_id": r.get::<_, String>(1)?, "matter_id": r.get::<_, String>(2)?, "display_name": r.get::<_, String>(3)?, "updated_at": r.get::<_, String>(4)?})))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Page view that first renders a display image for a native PDF page
    /// (format conversion of the stored original; nothing leaves the vault).
    pub fn render_page(&self, page: &str) -> Result<Value> {
        let handle = self.vault()?;
        handle.lock().map_err(|_| Error::Integrity)?.expect_kind(page, &["PageRef"])?;
        let cancel = AtomicBool::new(false);
        let sink = |_: Progress| {};
        let ctx = Ctx { vault: &handle, services: &self.services, cancel: &cancel, progress: &sink };
        handlers::documents::ensure_display_image(&ctx, page)?;
        self.page_view(page)
    }

    /// Media of a version: technical probe, decoded segments with transcripts
    /// and sampled frames (page ids for the frame OCR view).
    pub fn media_view(&self, version: &str) -> Result<Value> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.expect_kind(version, &["DocumentVersionRef"])?;
        let media: Option<(String, String, Option<f64>, i64, i64)> = vault
            .db()
            .query_row("SELECT id,probe,duration_s,has_audio,has_video FROM media WHERE version_id=?", [version], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
        let Some((media, probe, duration, audio, video)) = media else { return Ok(json!({"version_id": version, "media_id": null})) };
        let mut statement = vault.db().prepare("SELECT id,seq,start_s,end_s,transcript,status,truncated FROM audio_segments WHERE media_id=? ORDER BY seq")?;
        let segments: Vec<Value> = statement
            .query_map([&media], |r| Ok(json!({"segment_id": r.get::<_, String>(0)?, "seq": r.get::<_, i64>(1)?, "start_s": r.get::<_, f64>(2)?, "end_s": r.get::<_, f64>(3)?, "transcript": r.get::<_, Option<String>>(4)?, "status": r.get::<_, String>(5)?, "truncated": r.get::<_, Option<i64>>(6)?.map(|v| v != 0)})))?
            .collect::<std::result::Result<_, _>>()?;
        let mut statement = vault.db().prepare("SELECT f.id,f.seq,f.time_s,f.page_id,p.ocr_status FROM frames f LEFT JOIN pages p ON p.id=f.page_id WHERE f.media_id=? ORDER BY f.seq")?;
        let frames: Vec<Value> = statement
            .query_map([&media], |r| Ok(json!({"frame_id": r.get::<_, String>(0)?, "seq": r.get::<_, i64>(1)?, "time_s": r.get::<_, f64>(2)?, "page_id": r.get::<_, Option<String>>(3)?, "ocr_status": r.get::<_, Option<String>>(4)?})))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(json!({"version_id": version, "media_id": media, "probe": serde_json::from_str::<Value>(&probe).unwrap_or(Value::Null), "duration_s": duration,
            "has_audio": audio != 0, "has_video": video != 0, "segments": segments, "frames": frames}))
    }

    /// Trusted description of a prepared change for the native confirmation
    /// dialog: its type, the matter title and the impact computed at prepare
    /// time. The text shown to the user is built from these values only.
    // ------------------------------------------------- generation connections

    fn credential_store(&self) -> Result<crate::generation::credentials::CredentialStore> {
        let handle = self.vault()?;
        let vault_id = handle.lock().map_err(|_| Error::Integrity)?.id.clone();
        crate::generation::credentials::CredentialStore::for_vault(&self.services.layout.data_root, &vault_id)
    }

    /// Secure input channel: the key is sealed with DPAPI immediately and only
    /// its credential reference reaches the draft (never Skill input or logs).
    pub fn connection_draft(&self, input: &Value, key: Option<Zeroizing<String>>) -> Result<String> {
        let draft: crate::generation::connections::DraftInput = serde_json::from_value(input.clone()).map_err(|_| Error::Contract)?;
        let profile = draft.validate()?;
        let credential = match key.filter(|k| !k.trim().is_empty()) {
            Some(key) => {
                let key = Zeroizing::new(key.trim().to_owned());
                if key.len() > 512 || key.chars().any(|c| c.is_control() || c.is_whitespace()) {
                    return Err(Error::NeedsInput("api_key"));
                }
                let id = crate::generation::credentials::CredentialStore::new_id();
                self.credential_store()?.put(&id, &key)?;
                Some(id)
            }
            None if draft.connection_id.is_none() && !profile.endpoint.is_loopback() => return Err(Error::NeedsInput("api_key")),
            None => None,
        };
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        crate::generation::connections::create_draft(&vault, &draft, credential.as_deref())
    }

    /// A draft that refers to an existing connection unchanged (probe/revoke).
    pub fn connection_reference(&self, connection: &str) -> Result<String> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let loaded = crate::generation::connections::load(&vault, connection)?;
        if loaded.status != "active" {
            return Err(Error::Denied);
        }
        let input = crate::generation::connections::DraftInput {
            connection_id: Some(connection.into()), name: loaded.name.clone(), provider: loaded.profile.provider.as_str().into(), protocol: loaded.profile.protocol.as_str().into(),
            base_url: loaded.profile.endpoint.base_url(), model_id: loaded.profile.model_id.clone(), timeout_s: loaded.profile.timeout_s, max_output_tokens: loaded.profile.max_output_tokens,
            proxy: if loaded.profile.proxy == crate::generation::transport::ProxyMode::None { "none".into() } else { "system".into() },
        };
        crate::generation::connections::create_draft(&vault, &input, None)
    }

    pub fn connections(&self) -> Result<Vec<Value>> {
        let store = self.credential_store()?;
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        crate::generation::connections::summary(&vault, &|id| store.exists(id))
    }

    pub fn set_default_connection(&self, connection: &str) -> Result<()> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        crate::generation::connections::set_default(&vault, connection)
    }

    /// What the native confirmation for a probe or revoke shows.
    pub fn describe_connection(&self, connection: &str) -> Result<Value> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let loaded = crate::generation::connections::load(&vault, connection)?;
        Ok(json!({"name": loaded.name, "host": loaded.profile.endpoint.display_host(), "model_id": loaded.profile.model_id, "loopback": loaded.profile.endpoint.is_loopback()}))
    }

    // ------------------------------------------------------- egress (answers)

    /// Prepare an answer: run the local retrieval (no egress) to show what
    /// would be sent, and register an unapproved grant bound to the question,
    /// scope, connection and limits. Approval happens in a native dialog.
    pub fn answer_preview(&self, text: &str, matters: &[String], connection: Option<&str>) -> Result<Value> {
        let question = self.create_question(text)?;
        let scope = self.create_scope(matters, &[])?;
        let (connection_id, name, host, model, max_tokens) = {
            let handle = self.vault()?;
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            let id = match connection {
                Some(id) => id.to_owned(),
                None => crate::generation::connections::default_connection(&vault)?.ok_or(Error::NeedsInput("connection"))?,
            };
            let loaded = crate::generation::connections::load(&vault, &id)?;
            if loaded.status != "active" {
                return Err(Error::Denied);
            }
            if loaded.credential.is_none() && !loaded.profile.endpoint.is_loopback() {
                return Err(Error::NeedsInput("api_key"));
            }
            (id, loaded.name, loaded.profile.endpoint.display_host(), loaded.profile.model_id, loaded.profile.max_output_tokens)
        };
        // Local-only retrieval for the preview (plain UI root, no generation).
        let envelope = self.run_skill("lvjin-retrieval", json!({"question_id": question, "scope_id": scope, "known_constraint_ids": [], "requested_output": "evidence_only"}), "ui")?;
        let items = match envelope["data_ref"].as_str() {
            Some(id) => self.result(id)?["payload"]["items"].clone(),
            None => json!([]),
        };
        let excerpts: Vec<Value> = items.as_array().cloned().unwrap_or_default().iter().take(EGRESS_MAX_ITEMS as usize).map(|item| json!({
            "evidence_id": item["evidence_id"], "display_name": item["document"]["display_name"], "unit_title": item["unit_title"],
            "pages": item["pages"], "locators": item["locators"], "excerpt": item["exact_text"].as_str().unwrap_or("").chars().take(240).collect::<String>(),
        })).collect();
        let preview = json!({"question": text.trim(), "connection": {"id": connection_id, "name": name, "host": host, "model_id": model},
            "purpose": "answer", "excerpts": excerpts, "max_items": EGRESS_MAX_ITEMS, "max_requests": EGRESS_MAX_REQUESTS, "max_output_tokens": max_tokens,
            "validity": "this_task", "matters": matters.len()});
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let grant = vault.register("EgressGrantRef", "m2", Some(&question))?;
        vault.db().execute(
            "INSERT INTO egress_grants(id,connection_id,host,purpose,scope_id,question_id,max_items,max_requests,preview,created_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
            params![grant, connection_id, host, "answer", scope, question, EGRESS_MAX_ITEMS, EGRESS_MAX_REQUESTS, preview.to_string(), now_rfc3339()],
        )?;
        let mut out = preview;
        out["grant_id"] = json!(grant);
        out["question_id"] = json!(question);
        out["scope_id"] = json!(scope);
        Ok(out)
    }

    /// The real parameters of an unapproved grant (for the native dialog).
    pub fn describe_grant(&self, grant: &str) -> Result<Value> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.expect_kind(grant, &["EgressGrantRef"])?;
        let (preview, approved, root): (String, Option<String>, Option<String>) =
            vault.db().query_row("SELECT preview,approved_at,root FROM egress_grants WHERE id=? AND revoked_at IS NULL", [grant], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.ok_or(Error::Denied)?;
        if approved.is_some() || root.is_some() {
            return Err(Error::Denied);
        }
        Ok(serde_json::from_str(&preview)?)
    }

    /// Called only after the native confirmation; then starts the answer task
    /// under a confirmed root that holds generation.request and this grant.
    pub fn answer_start(self: &Arc<Self>, grant: &str, excluded: &[String]) -> Result<String> {
        let (question, scope) = {
            let handle = self.vault()?;
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            let (question, scope): (String, String) = vault.db().query_row("SELECT question_id,scope_id FROM egress_grants WHERE id=? AND approved_at IS NULL AND root IS NULL AND revoked_at IS NULL", [grant], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
            if !excluded.is_empty() {
                // Excluded excerpts tighten the grant: they are removed from the
                // scope by version so they cannot be sent in this task.
                let mut preview: Value = serde_json::from_str(&vault.db().query_row::<String, _, _>("SELECT preview FROM egress_grants WHERE id=?", [grant], |r| r.get(0))?)?;
                preview["excluded"] = json!(excluded);
                vault.db().execute("UPDATE egress_grants SET preview=? WHERE id=?", params![preview.to_string(), grant])?;
            }
            vault.db().execute("UPDATE egress_grants SET approved_at=? WHERE id=?", params![now_rfc3339(), grant])?;
            (question, scope)
        };
        let input = json!({"question_id": question, "scope_id": scope, "known_constraint_ids": [], "requested_output": "answer"});
        let (handle, invocation) = self.begin_with("lvjin-retrieval", &input, "confirm", Some(grant))?;
        self.services.log.event("egress.approved", &[("purpose", crate::logging::Field::Code("answer"))]);
        self.spawn("lvjin-retrieval", input, handle, invocation)
    }

    pub fn answer_cancel_preview(&self, grant: &str) -> Result<()> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.db().execute("UPDATE egress_grants SET revoked_at=? WHERE id=? AND approved_at IS NULL", params![now_rfc3339(), grant])?;
        Ok(())
    }

    /// Requests sent to generation services (audit view; no content).
    pub fn generation_requests(&self, limit: u32) -> Result<Vec<Value>> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let mut statement = vault.db().prepare("SELECT r.purpose,r.host,r.model_id,r.status,r.error,r.input_tokens,r.output_tokens,r.materials,r.started_at,c.name FROM generation_requests r LEFT JOIN connections c ON c.id=r.connection_id ORDER BY r.started_at DESC LIMIT ?")?;
        let rows = statement.query_map([limit.min(500)], |r| Ok(json!({"purpose": r.get::<_, String>(0)?, "host": r.get::<_, String>(1)?, "model_id": r.get::<_, String>(2)?, "status": r.get::<_, String>(3)?,
            "error": r.get::<_, Option<String>>(4)?, "input_tokens": r.get::<_, Option<i64>>(5)?, "output_tokens": r.get::<_, Option<i64>>(6)?,
            "materials": serde_json::from_str::<Value>(&r.get::<_, String>(7)?).unwrap_or(Value::Null), "started_at": r.get::<_, String>(8)?, "connection": r.get::<_, Option<String>>(9)?})))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn describe_change(&self, change_ref: &str) -> Result<Value> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.expect_kind(change_ref, &["PreparedChangeRef"])?;
        let (matter, change, impact, status, title): (String, String, String, String, String) = vault.db().query_row(
            "SELECT c.matter_id,c.change,c.impact,c.status,m.title FROM changes c JOIN matters m ON m.id=c.matter_id WHERE c.id=?",
            [change_ref],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let change: Value = serde_json::from_str(&change)?;
        let mut description = json!({"change_ref": change_ref, "matter_id": matter, "matter_title": title, "type": change["type"], "status": status, "impact": serde_json::from_str::<Value>(&impact)?});
        if let Some(version) = change["version_id"].as_str() {
            let document: Option<(String, i64)> = vault
                .db()
                .query_row("SELECT d.display_name,v.version_no FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?", [version], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            if let Some((name, number)) = document {
                description["document"] = json!({"display_name": name, "version_no": number});
            }
        }
        for field in ["fact_ids", "cashflow_ids"] {
            if let Some(ids) = change[field].as_array() {
                description["count"] = json!(ids.len());
            }
        }
        if let Some(title) = change["title"].as_str() {
            description["title"] = json!(title);
        }
        Ok(description)
    }

    /// Registered Skills and their M1 status for the "功能与Skill" page.
    pub fn skills(&self) -> Result<Vec<Value>> {
        let registry = self.registry()?;
        Ok(registry
            .skills
            .values()
            .map(|s| json!({"id": s.id, "title": s.title, "kind": s.kind, "description": s.description, "version": s.version, "implementation_status": s.implementation_status,
                "actors": s.actors, "direct_capabilities": s.direct_capabilities, "conditional_capabilities": s.conditional_capabilities, "children": s.children}))
            .collect())
    }

    /// The nine bundled CPU models (names, revisions, licenses) for "关于".
    pub fn models(&self) -> Result<Value> {
        let manifest: Value = serde_json::from_slice(&std::fs::read(self.services.layout.resources.join("models/manifest.json"))?)?;
        let models: Vec<Value> = manifest["models"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|m| json!({"model_id": m["model_id"], "source_model_id": m["source_model_id"], "revision": m["source_revision"], "license": m["license"],
                "installed_bytes": m["installed_bytes"], "compute": m["compute_profile"]}))
            .collect();
        Ok(json!({"models": models, "gpu_enabled": false, "residency": {"max_large": self.services.residency.max_large, "coresident": self.services.residency.coresident}}))
    }

    pub fn object_exists(&self, id: &str) -> Result<bool> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        Ok(vault.db().query_row("SELECT active FROM objects WHERE id=?", [id], |r| r.get::<_, i64>(0)).optional()?.is_some_and(|v| v == 1))
    }
}

/// Read-only inspection for the synthetic M1 tests and benchmarks. Compiled
/// only with `m0-harness`; production builds have no raw SQL entry point.
#[cfg(feature = "m0-harness")]
impl CoreService {
    pub fn harness_rows(&self, sql: &str, args: &[&str]) -> Result<Vec<Vec<Value>>> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let mut statement = vault.db().prepare(sql)?;
        let width = statement.column_count();
        let rows = statement.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            (0..width)
                .map(|index| {
                    Ok(match r.get::<_, rusqlite::types::Value>(index)? {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(v) => json!(v),
                        rusqlite::types::Value::Real(v) => json!(v),
                        rusqlite::types::Value::Text(v) => json!(v),
                        rusqlite::types::Value::Blob(v) => json!(v.len()),
                    })
                })
                .collect::<std::result::Result<Vec<_>, _>>()
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn harness_count(&self, sql: &str, args: &[&str]) -> Result<i64> {
        self.harness_rows(sql, args)?.first().and_then(|r| r.first()).and_then(Value::as_i64).ok_or(Error::Integrity)
    }

    /// Encrypted object files currently registered in the unlocked vault.
    pub fn harness_blob_files(&self) -> Result<Vec<(String, PathBuf)>> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.blob_files()
    }

    pub fn harness_vault_dir(&self) -> Result<PathBuf> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        Ok(vault.dir.clone())
    }
}

struct HashWriter<'a>(&'a mut sha2::Sha256);
impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        sha2::Digest::update(self.0, buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn wav_from_f32(bytes: &[u8]) -> Vec<u8> {
    let samples: Vec<i16> = bytes
        .chunks_exact(4)
        .map(|c| (f32::from_le_bytes([c[0], c[1], c[2], c[3]]).clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16000u32.to_le_bytes());
    out.extend_from_slice(&32000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}
