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
use base64::Engine as _;

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
/// Native media prepared for an approved request: JPEG, longest side 2048 px.
const MEDIA_MAX_SIDE: u32 = 2048;
const MEDIA_QUALITY: u32 = 85;
/// Page thumbnails: longest side (the local encoder's minimum) and quality.
const THUMB_SIDE: u32 = 256;
const THUMB_QUALITY: u32 = 70;
/// Longest audio a grant may approve for a direct listening request.
pub const AUDIO_MAX_SECONDS: f64 = 300.0;
/// Auto-lock waits for a task only while it keeps reporting progress, and
/// never longer than the cap after the idle time ran out.
const LOCK_STALL: Duration = Duration::from_secs(5 * 60);
const LOCK_MAX_DEFER: Duration = Duration::from_secs(30 * 60);
/// How long locking waits for task threads before they finish on their own.
const LOCK_JOIN: Duration = Duration::from_secs(10);
/// Finished tasks kept for the task centre during one session.
const KEEP_FINISHED: usize = 200;

/// Where the excerpts shown in a preview come from.
enum Excerpts {
    /// Local hybrid retrieval of the question (no generation).
    Retrieval,
    /// The indexed text of the selected pages.
    Pages(Vec<String>),
    /// Produced locally during the run (transcripts, frame OCR).
    Later,
}

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
    /// A failed or stopped interface task that can be started again as it was.
    pub retryable: bool,
}

struct Task {
    state: TaskState,
    cancel: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Start or last progress report: auto-lock waits only for moving tasks.
    moved: Instant,
    /// Input of a local task the user started from the interface (no grant,
    /// no confirmation): `retry_task` starts it again with the same input.
    retry: Option<Value>,
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
    /// When auto-lock first waited for running tasks (cleared by activity).
    deferred: Mutex<Option<Instant>>,
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
            deferred: Mutex::new(None),
            failures: Mutex::new((0, None)),
        }))
    }

    /// User activity (input in the window). Only this keeps the vault from
    /// auto-locking; background reads and task progress do not.
    pub fn touch(&self) {
        if let Ok(mut last) = self.activity.lock() {
            *last = Instant::now();
        }
        if let Ok(mut deferred) = self.deferred.lock() {
            *deferred = None;
        }
    }

    fn vault(&self) -> Result<VaultHandle> {
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
        // Flow runs cut off by an exit continue only after a new confirmation;
        // a request that left without a recorded reply has an unknown outcome.
        opened.db().execute("UPDATE flow_runs SET status='interrupted',reason='app_exit',updated_at=? WHERE status IN ('running','resuming')", [now_rfc3339()])?;
        opened.db().execute("UPDATE generation_requests SET status='unknown',finished_at=? WHERE status='sent'", [now_rfc3339()])?;
        handlers::observe::sweep_native(&opened)?;
        self.install_session(handle.into(), opened)?;
        // API keys sealed with the earlier public entropy move to the secret one.
        if let Ok(migrated) = self.credential_store().and_then(|store| store.migrate_legacy()) {
            if migrated > 0 {
                self.services.log.event("credential.migrated", &[("count", crate::logging::Field::Count(migrated as i64))]);
            }
        }
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

    /// Lock: stop tasks, forget the session and view tokens at once, then give
    /// task threads a bounded time to end and stop the sensitive workers.
    pub fn lock(&self) -> Result<()> {
        let ids: Vec<String> = self.tasks.lock().map_err(|_| Error::Integrity)?.keys().cloned().collect();
        for id in &ids {
            // A running flow is interrupted, not cancelled: its root stays valid
            // so the user may continue it after unlocking (with a new confirmation).
            if self.task_flow_running(id).unwrap_or(false) {
                let _ = self.interrupt_task(id);
            } else {
                let _ = self.cancel_task(id);
            }
        }
        if self.is_unlocked() {
            let _ = self.protected_audit("lock");
        }
        // From here no command reaches the vault, whatever a task thread is
        // still finishing; the keys go when the last handle is dropped.
        let session = self.session.lock().map_err(|_| Error::Integrity)?.take();
        self.tokens.lock().map_err(|_| Error::Integrity)?.clear();
        // Stopping the workers also ends any task thread waiting for them.
        self.services.stop_sensitive();
        let threads: Vec<std::thread::JoinHandle<()>> = {
            let mut tasks = self.tasks.lock().map_err(|_| Error::Integrity)?;
            tasks.values_mut().filter_map(|t| t.thread.take()).collect()
        };
        let deadline = Instant::now() + LOCK_JOIN;
        let mut detached = 0i64;
        for thread in threads {
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                detached += 1;
            }
        }
        drop(session);
        self.tasks.lock().map_err(|_| Error::Integrity)?.clear();
        if let Ok(mut deferred) = self.deferred.lock() {
            *deferred = None;
        }
        self.services.stop_sensitive();
        self.services.log.event("vault.locked", &[("detached", crate::logging::Field::Count(detached))]);
        Ok(())
    }

    /// Whether a Windows session lock or sleep locks the vault (user setting,
    /// on by default). Never waits for a busy vault: then it says yes.
    pub fn lock_on_system_lock(&self) -> bool {
        let Ok(session) = self.session.lock() else { return false };
        let Some(handle) = session.as_ref().map(|s| s.vault.clone()) else { return false };
        drop(session);
        let Ok(vault) = handle.try_lock() else { return true };
        vault.setting("lock_on_system_lock").ok().flatten().map(|value| value != "0").unwrap_or(true)
    }

    /// The lock settings shown on the settings page (reading is not activity).
    pub fn lock_policy(&self) -> Result<Value> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let minutes: u64 = vault.setting("auto_lock_minutes")?.and_then(|v| v.parse().ok()).unwrap_or(15).max(1);
        let system = vault.setting("lock_on_system_lock")?.map(|value| value != "0").unwrap_or(true);
        Ok(json!({"auto_lock_minutes": minutes, "lock_on_system_lock": system}))
    }

    /// Called periodically by the shell; locks after the configured idle time.
    /// Tasks that keep reporting progress may delay the lock, at most by
    /// `LOCK_MAX_DEFER`; a task without progress for `LOCK_STALL` does not.
    pub fn auto_lock_tick(&self) -> Result<bool> {
        // Reading the policy is not user activity (it must not touch).
        let Some(handle) = self.session.lock().map_err(|_| Error::Integrity)?.as_ref().map(|s| s.vault.clone()) else { return Ok(false) };
        let minutes: u64 = handle.lock().map_err(|_| Error::Integrity)?.setting("auto_lock_minutes")?.and_then(|v| v.parse().ok()).unwrap_or(15).max(1);
        let idle = self.activity.lock().map_err(|_| Error::Integrity)?.elapsed();
        if idle < Duration::from_secs(minutes * 60) {
            return Ok(false);
        }
        let moving = self.tasks.lock().map_err(|_| Error::Integrity)?.values().any(|t| t.state.status == "running" && t.moved.elapsed() < LOCK_STALL);
        let waited = self.deferred.lock().map_err(|_| Error::Integrity)?.get_or_insert_with(Instant::now).elapsed();
        if moving && waited < LOCK_MAX_DEFER {
            return Ok(false);
        }
        self.lock()?;
        Ok(true)
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
        let retry = (action == "ui").then(|| input.clone());
        self.spawn_with(skill, input, handle, invocation, retry)
    }

    /// Start a failed or stopped interface task again with its original input
    /// (a new root; imports are idempotent per file selection).
    pub fn retry_task(self: &Arc<Self>, id: &str) -> Result<String> {
        let (skill, input) = {
            let tasks = self.tasks.lock().map_err(|_| Error::Integrity)?;
            let task = tasks.get(id).ok_or(Error::Denied)?;
            if !task.state.retryable {
                return Err(Error::Denied);
            }
            (task.state.skill.clone(), task.retry.clone().ok_or(Error::Denied)?)
        };
        let started = self.start_skill(&skill, input, "ui")?;
        if let Some(task) = self.tasks.lock().map_err(|_| Error::Integrity)?.get_mut(id) {
            task.state.retryable = false;
        }
        Ok(started)
    }

    fn spawn(self: &Arc<Self>, skill: &str, input: Value, handle: VaultHandle, invocation: crate::runtime::Invocation) -> Result<String> {
        self.spawn_with(skill, input, handle, invocation, None)
    }

    fn spawn_with(self: &Arc<Self>, skill: &str, input: Value, handle: VaultHandle, invocation: crate::runtime::Invocation, retry: Option<Value>) -> Result<String> {
        let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());
        let cancel = Arc::new(AtomicBool::new(false));
        let state = TaskState {
            task_id: task_id.clone(), skill: skill.into(), root: invocation.root_id().into(), status: "running".into(), progress: None, envelope: None, started_at: now_rfc3339(), retryable: false,
        };
        {
            let mut tasks = self.tasks.lock().map_err(|_| Error::Integrity)?;
            // Keep the most recent finished tasks only; running ones always stay.
            let mut finished: Vec<(String, String)> = tasks.iter().filter(|(_, t)| t.state.status != "running").map(|(id, t)| (t.state.started_at.clone(), id.clone())).collect();
            if finished.len() >= KEEP_FINISHED {
                finished.sort();
                let excess = finished.len() + 1 - KEEP_FINISHED;
                for (_, id) in finished.into_iter().take(excess) {
                    tasks.remove(&id);
                }
            }
            tasks.insert(task_id.clone(), Task { state, cancel: cancel.clone(), thread: None, moved: Instant::now(), retry });
        }
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
                        task.moved = Instant::now();
                    }
                }
                sink(TaskEvent { task_id: progress_id.clone(), kind: "progress", progress: Some(progress), envelope: None });
            };
            let ctx = Ctx { vault: &handle, services: &service.services, cancel: &cancel, progress: &on_progress };
            // A cancelled root can no longer seal an envelope; report it as
            // cancelled. A panic ends the task as failed instead of leaving it
            // "running" (which would also hold off the auto-lock).
            let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handlers::execute(&ctx, &invocation, &input)));
            let mut envelope = match executed {
                Ok(result) => result.unwrap_or_else(|error| match error {
                    Error::Cancelled => json!({"status": "cancelled", "warnings": [{"code": "cancelled", "message": "任务已停止、撤销或空间已锁定。"}]}),
                    other => json!({"status": "failed", "warnings": [{"code": other.code(), "message": "执行失败"}]}),
                }),
                Err(_) => json!({"status": "failed", "warnings": [{"code": "internal_error", "message": "执行异常结束；请锁定后重新解锁再试。"}]}),
            };
            // A flow stopped by lock, exit or a vanished orchestrator keeps its
            // root; it can be continued after a new confirmation.
            if envelope["status"] == "cancelled" || envelope["status"] == "failed" {
                if let Some(run) = interrupted_run(&handle, invocation.root_id()) {
                    envelope = json!({"status": "interrupted", "flow_run": run, "warnings": [{"code": "flow_interrupted", "message": "任务已中断；可在确认后从中断处继续。"}]});
                }
            }
            // Model copies of approved media live only as long as the task can
            // still send them; an interrupted run keeps them for its continuation.
            if envelope["status"] != "interrupted" {
                if let Ok(vault) = handle.lock() {
                    let _ = handlers::observe::release_root_native(&vault, invocation.root_id());
                }
            }
            if let Ok(mut tasks) = service.tasks.lock() {
                if let Some(task) = tasks.get_mut(&id) {
                    task.state.status = envelope["status"].as_str().unwrap_or("failed").into();
                    task.state.envelope = Some(envelope.clone());
                    task.state.retryable = task.retry.is_some() && matches!(task.state.status.as_str(), "failed" | "cancelled");
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

    fn task_flow_running(&self, id: &str) -> Result<bool> {
        let root = self.tasks.lock().map_err(|_| Error::Integrity)?.get(id).map(|t| t.state.root.clone()).ok_or(Error::Denied)?;
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        Ok(vault.db().query_row("SELECT EXISTS(SELECT 1 FROM flow_runs WHERE root=? AND status IN ('running','resuming'))", [&root], |r| r.get(0))?)
    }

    /// Stop a task's threads without cancelling its root (lock or app exit).
    pub fn interrupt_task(&self, id: &str) -> Result<()> {
        let tasks = self.tasks.lock().map_err(|_| Error::Integrity)?;
        tasks.get(id).ok_or(Error::Denied)?.cancel.store(true, Ordering::Release);
        Ok(())
    }

    // ------------------------------------------------------- flow continuation

    /// Interrupted flow runs the user may continue or discard.
    pub fn interrupted_flows(&self) -> Result<Vec<Value>> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let current: i64 = vault.db().query_row("SELECT COALESCE((SELECT generation FROM index_state WHERE id=1),0)", [], |r| r.get(0))?;
        let mut statement = vault.db().prepare(
            "SELECT f.id,f.root,f.skill,f.flow,f.owner_input,f.snapshot_generation,f.attempts,f.reason,f.created_at,f.updated_at,
                    (SELECT COUNT(*) FROM generation_requests g WHERE g.root=f.root AND (g.status='unknown' OR (g.status='failed' AND g.error='cancelled')))
             FROM flow_runs f WHERE f.status='interrupted' ORDER BY f.updated_at DESC",
        )?;
        let rows = statement.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?,
                r.get::<_, Option<String>>(7)?, r.get::<_, String>(8)?, r.get::<_, String>(9)?, r.get::<_, i64>(10)?))
        })?;
        let mut out = vec![];
        for row in rows {
            let (run, root, skill, flow, owner_input, generation, attempts, reason, created, updated, unknown) = row?;
            let input: Value = serde_json::from_str(&owner_input)?;
            let question: Option<String> = match input["question_id"].as_str() {
                Some(id) => vault.db().query_row("SELECT text FROM questions WHERE id=?", [id], |r| r.get(0)).optional()?,
                None => None,
            };
            out.push(json!({"run_id": run, "root": root, "skill": skill, "flow": flow, "question": question, "attempts": attempts, "reason": reason,
                "unknown_requests": unknown, "stale": generation != current, "created_at": created, "updated_at": updated}));
        }
        Ok(out)
    }

    /// The real parameters of a continuation, for the native dialog.
    pub fn describe_continue(&self, run: &str) -> Result<Value> {
        let listed = self.interrupted_flows()?.into_iter().find(|r| r["run_id"] == run).ok_or(Error::Denied)?;
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let root = listed["root"].as_str().ok_or(Error::Integrity)?;
        let (connection, host, max_requests): (String, String, i64) = vault
            .db()
            .query_row("SELECT connection_id,host,max_requests FROM egress_grants WHERE root=? AND approved_at IS NOT NULL AND revoked_at IS NULL", [root], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?
            .ok_or(Error::Denied)?;
        let loaded = crate::generation::connections::load(&vault, &connection)?;
        let used: i64 = vault.db().query_row("SELECT generation FROM roots WHERE id=?", [root], |r| r.get(0))?;
        let mut out = listed;
        out["connection"] = json!({"name": loaded.name, "host": host, "model_id": loaded.profile.model_id, "active": loaded.status == "active"});
        out["used_requests"] = json!(used);
        out["max_requests"] = json!(max_requests.min(7));
        Ok(out)
    }

    /// Continue an interrupted run after the native confirmation: same root,
    /// same counters, finished steps replayed from receipts. A changed
    /// material snapshot makes the run stale instead.
    pub fn flow_continue(self: &Arc<Self>, run: &str) -> Result<String> {
        let described = self.describe_continue(run)?;
        if described["connection"]["active"] != json!(true) {
            return Err(Error::Denied);
        }
        let handle = self.vault()?;
        let (invocation, input, skill) = {
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            if described["stale"] == json!(true) {
                vault.db().execute("UPDATE flow_runs SET status='stale',updated_at=? WHERE id=? AND status='interrupted'", params![now_rfc3339(), run])?;
                return Err(Error::NeedsInput("rerun"));
            }
            let (owner, owner_input, skill): (String, String, String) = vault.db().query_row("SELECT owner,owner_input,skill FROM flow_runs WHERE id=? AND status='interrupted'", [run], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.ok_or(Error::Denied)?;
            let invocation = vault.runtime.reload_invocation(&owner)?;
            let changed = vault.db().execute("UPDATE flow_runs SET status='resuming',attempts=attempts+1,updated_at=? WHERE id=? AND status='interrupted'", params![now_rfc3339(), run])?;
            if changed != 1 {
                return Err(Error::Conflict);
            }
            (invocation, serde_json::from_str::<Value>(&owner_input)?, skill)
        };
        self.services.log.event("flow.continue", &[("skill", crate::logging::Field::Status(skill.clone()))]);
        self.spawn(&skill, input, handle, invocation)
    }

    /// Give up an interrupted run: its root is cancelled and its grant revoked.
    pub fn flow_discard(&self, run: &str) -> Result<()> {
        let handle = self.vault()?;
        let mut vault = handle.lock().map_err(|_| Error::Integrity)?;
        let root: String = vault.db().query_row("SELECT root FROM flow_runs WHERE id=? AND status IN ('interrupted','stale')", [run], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
        vault.db().execute("UPDATE flow_runs SET status='cancelled',reason='discarded',updated_at=? WHERE id=?", params![now_rfc3339(), run])?;
        vault.db().execute("UPDATE egress_grants SET revoked_at=? WHERE root=? AND revoked_at IS NULL", params![now_rfc3339(), root])?;
        handlers::observe::release_root_native(&vault, &root)?;
        vault.runtime.cancel_root(&root)
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

    /// "依据有误？": the user's own note on a result (which sentence, what is
    /// wrong). It stays in the vault, is never sent, and is shown with the
    /// result and in its copies.
    pub fn create_feedback(&self, result: &str, claim: Option<&str>, issue: &str, note: &str) -> Result<String> {
        let note = note.trim();
        if !matches!(issue, "unsupported" | "wrong_source" | "contradiction" | "other") || note.chars().count() > 500 || claim.is_some_and(|c| c.is_empty() || c.len() > 80) {
            return Err(Error::Contract);
        }
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.get_result(result)?;
        let id = vault.register("FeedbackRef", "m2", Some(result))?;
        vault.db().execute(
            "INSERT INTO feedback(id,result_ref,claim_id,issue,note,created_at) VALUES(?,?,?,?,?,?)",
            params![id, result, claim, issue, (!note.is_empty()).then_some(note), now_rfc3339()],
        )?;
        self.services.log.event("feedback.recorded", &[("issue", crate::logging::Field::Status(issue.to_owned()))]);
        Ok(id)
    }

    pub fn feedback(&self, result: &str) -> Result<Value> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let mut statement = vault.db().prepare("SELECT f.id,f.claim_id,f.issue,f.note,f.created_at FROM feedback f JOIN objects o ON o.id=f.id WHERE f.result_ref=? AND o.active=1 ORDER BY f.created_at")?;
        let rows = statement.query_map([result], |r| {
            Ok(json!({"id": r.get::<_, String>(0)?, "claim_id": r.get::<_, Option<String>>(1)?, "issue": r.get::<_, String>(2)?, "note": r.get::<_, Option<String>>(3)?, "created_at": r.get::<_, String>(4)?}))
        })?;
        Ok(json!(rows.collect::<std::result::Result<Vec<_>, _>>()?))
    }

    pub fn remove_feedback(&self, id: &str) -> Result<()> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.expect_kind(id, &["FeedbackRef"])?;
        vault.db().execute("DELETE FROM feedback WHERE id=?", [id])?;
        vault.db().execute("UPDATE objects SET active=0 WHERE id=?", [id])?;
        Ok(())
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

    /// A small JPEG of one page for the inspector's page strip (design 9.4),
    /// made locally from the page's display image (native PDF pages are
    /// rendered first, as when the page is opened) and kept as an encrypted
    /// blob owned by the page, so it is deleted with the version.
    pub fn page_thumb(&self, page: &str) -> Result<Value> {
        let handle = self.vault()?;
        let cached = |vault: &Vault| -> Result<Option<String>> {
            Ok(vault.db().query_row("SELECT id FROM blobs WHERE owner=? AND purpose='page_thumb'", [page], |r| r.get::<_, String>(0)).optional()?)
        };
        let existing = {
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            vault.expect_kind(page, &["PageRef"])?;
            cached(&vault)?
        };
        let blob = match existing {
            Some(blob) => blob,
            None => {
                let cancel = AtomicBool::new(false);
                let sink = |_: Progress| {};
                let ctx = Ctx { vault: &handle, services: &self.services, cancel: &cancel, progress: &sink };
                let display = handlers::documents::ensure_display_image(&ctx, page)?;
                let image = handle.lock().map_err(|_| Error::Integrity)?.get_blob(&display)?;
                let encoded = self.services.local.call(&json!({"op": "encode_for_model", "png_base64": base64::engine::general_purpose::STANDARD.encode(&*image), "max_side": THUMB_SIDE, "quality": THUMB_QUALITY}), &cancel, Duration::from_secs(60))?;
                let jpeg = base64::engine::general_purpose::STANDARD.decode(encoded["jpeg_base64"].as_str().ok_or(Error::Contract)?).map_err(|_| Error::Contract)?;
                let vault = handle.lock().map_err(|_| Error::Integrity)?;
                // Two strips may have asked for the same page at once.
                match cached(&vault)? {
                    Some(blob) => blob,
                    None => vault.put_blob(page, "page_thumb", &jpeg)?,
                }
            }
        };
        Ok(json!({"page_id": page, "token": self.issue_token(&blob, "image/jpeg")?}))
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
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        crate::generation::credentials::CredentialStore::for_vault(&self.services.layout.data_root, &vault.id, &vault.keys.credential)
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
        self.answer_preview_with(text, matters, connection, false)
    }

    /// Q&A preview; `deep` approves the planner and narrow roles ("深入分析").
    pub fn answer_preview_with(&self, text: &str, matters: &[String], connection: Option<&str>, deep: bool) -> Result<Value> {
        let question = self.create_question(text)?;
        let scope = self.create_scope(matters, &[])?;
        let purpose = if deep { "deep_answer" } else { "answer" };
        let input = json!({"question_id": question, "scope_id": scope, "known_constraint_ids": [], "requested_output": "answer"});
        self.grant_preview(purpose, text, &question, &scope, matters.len(), connection, input, Excerpts::Retrieval, json!({}))
    }

    /// Preview of a written analysis (contract, loan or budget): its fixed
    /// roles, the excerpts likely sent and the request limits.
    pub fn analysis_preview(&self, kind: &str, request: &Value) -> Result<Value> {
        let text = request["question"].as_str().filter(|t| !t.trim().is_empty()).ok_or(Error::NeedsInput("question"))?;
        let versions: Vec<String> = request["document_versions"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        let matter = request["matter_id"].as_str().map(str::to_owned);
        let question = self.create_question(text)?;
        let (purpose, input, matters) = match kind {
            "contract-review" => {
                if versions.is_empty() {
                    return Err(Error::NeedsInput("document_versions"));
                }
                ("contract_review", json!({"document_versions": versions, "question_id": question, "known_constraint_ids": [], "media_refs": []}), matter.clone().into_iter().collect::<Vec<_>>())
            }
            "loan-analysis" => {
                let matter = matter.clone().ok_or(Error::NeedsInput("matter_id"))?;
                ("loan_analysis", json!({"matter_id": matter, "document_versions": versions, "question_id": question, "media_refs": []}), vec![matter])
            }
            "budget-analysis" => {
                if versions.is_empty() {
                    return Err(Error::NeedsInput("document_versions"));
                }
                ("budget_analysis", json!({"document_versions": versions, "period": request["period"], "account_ids": request["account_ids"].as_array().cloned().unwrap_or_default(), "question_id": question}),
                    matter.clone().into_iter().collect::<Vec<_>>())
            }
            _ => return Err(Error::Contract),
        };
        let scope = self.create_scope(&matters, &versions)?;
        let roles = match purpose {
            "budget_analysis" => json!(["金融口径分析者", "声明证据检查者", "结果综合者"]),
            _ => json!(["法律适用分析者", "金融口径分析者", "声明证据检查者", "结果综合者"]),
        };
        self.grant_preview(purpose, text, &question, &scope, matters.len().max(1), request["connection"].as_str(), input, Excerpts::Retrieval, json!({"roles": roles}))
    }

    /// Preview of a media review: approved images are prepared now (JPEG,
    /// longest side 2048 px, listed one by one); for audio and video the grant
    /// approves a bounded policy (frames of the range, at most 12; the range's
    /// audio, at most 300 s), prepared by the core during the run.
    pub fn media_preview(&self, kind: &str, request: &Value) -> Result<Value> {
        let text = request["question"].as_str().filter(|t| !t.trim().is_empty()).ok_or(Error::NeedsInput("question"))?;
        let question = self.create_question(text)?;
        let connection_id = self.connection_for(request["connection"].as_str())?;
        let (vision, audio_input) = {
            let handle = self.vault()?;
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            let connection = crate::generation::connections::load(&vault, &connection_id)?;
            (crate::generation::connections::capability(&vault, &connection, "vision")? == "supported",
             crate::generation::connections::capability(&vault, &connection, "audio_input")? == "supported")
        };
        match kind {
            "image-review" => {
                let images: Vec<String> = request["image_refs"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
                if images.is_empty() || images.len() > crate::handlers::observe::MAX_IMAGES_PER_REQUEST {
                    return Err(Error::NeedsInput("image_refs"));
                }
                let (pages, versions) = self.image_pages(&images)?;
                let scope = self.create_scope(&[], &versions)?;
                let input = json!({"image_refs": images, "question_id": question, "mode": "analyze"});
                let mut preview = self.grant_preview("image_review", text, &question, &scope, 0, Some(&connection_id), input, Excerpts::Pages(pages.clone()), json!({}))?;
                if vision {
                    let grant = preview["grant_id"].as_str().ok_or(Error::Integrity)?.to_owned();
                    let media = self.prepare_images(&grant, &pages)?;
                    preview["native_media"] = json!(media);
                    self.update_preview(&grant, &preview)?;
                } else {
                    preview["native_media_note"] = json!("该连接没有通过图片能力自检；只发送本机识别出的文字，不上传图片。");
                    self.update_preview(preview["grant_id"].as_str().ok_or(Error::Integrity)?, &preview)?;
                }
                Ok(preview)
            }
            "audio-review" | "video-review" => {
                let source = request["source_ref"].as_str().ok_or(Error::NeedsInput("source_ref"))?.to_owned();
                let (version, media_kind): (String, String) = {
                    let handle = self.vault()?;
                    let vault = handle.lock().map_err(|_| Error::Integrity)?;
                    vault.expect_kind(&source, &["MediaVersionRef"])?;
                    vault.db().query_row("SELECT v.id,v.media_kind FROM media m JOIN document_versions v ON v.id=m.version_id WHERE m.id=?", [&source], |r| Ok((r.get(0)?, r.get(1)?)))?
                };
                if (kind == "video-review") != (media_kind == "video") {
                    return Err(Error::Contract);
                }
                let start = request["range"]["start_s"].as_f64().unwrap_or(0.0).max(0.0);
                let end = request["range"]["end_s"].as_f64();
                let range = self.create_range(&source, start, end)?;
                let scope = self.create_scope(&[], std::slice::from_ref(&version))?;
                let purpose = if kind == "video-review" { "video_review" } else { "audio_review" };
                let input = json!({"source_ref": source, "range_ref": range, "question_id": question, "analysis_goal": "analyze"});
                let policy = json!({"frames": if kind == "video-review" && vision { json!(crate::handlers::observe::MAX_IMAGES_PER_REQUEST) } else { json!(0) },
                    "audio_seconds": if audio_input { json!(AUDIO_MAX_SECONDS) } else { json!(0) }, "range": {"start_s": start, "end_s": end}});
                self.grant_preview(purpose, text, &question, &scope, 0, Some(&connection_id), input, Excerpts::Later, json!({"media_policy": policy}))
            }
            _ => Err(Error::Contract),
        }
    }

    fn connection_for(&self, connection: Option<&str>) -> Result<String> {
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
        Ok(id)
    }

    /// Pages behind image references (pages or video frames) and their versions.
    fn image_pages(&self, images: &[String]) -> Result<(Vec<String>, Vec<String>)> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let (mut pages, mut versions) = (vec![], vec![]);
        for image in images {
            let page = match vault.object_kind(image)?.as_str() {
                "PageRef" => image.clone(),
                "ImageRef" => vault.db().query_row("SELECT page_id FROM frames WHERE id=?", [image], |r| r.get::<_, String>(0))?,
                _ => return Err(Error::Denied),
            };
            let version: String = vault.db().query_row("SELECT version_id FROM pages WHERE id=?", [&page], |r| r.get(0))?;
            if !versions.contains(&version) {
                versions.push(version);
            }
            pages.push(page);
        }
        Ok((pages, versions))
    }

    /// Model copies of page images, bound to the (unapproved) grant.
    fn prepare_images(&self, grant: &str, pages: &[String]) -> Result<Vec<Value>> {
        let handle = self.vault()?;
        let cancel = AtomicBool::new(false);
        let mut out = vec![];
        for page in pages {
            let (blob, page_no, display): (String, i64, String) = {
                let vault = handle.lock().map_err(|_| Error::Integrity)?;
                vault.db().query_row(
                    "SELECT p.image_blob,p.page_no,d.display_name FROM pages p JOIN document_versions v ON v.id=p.version_id JOIN documents d ON d.id=v.document_id WHERE p.id=? AND p.image_blob IS NOT NULL",
                    [page], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            };
            let png = handle.lock().map_err(|_| Error::Integrity)?.get_blob(&blob)?;
            let encoded = self.services.local.call(&json!({"op": "encode_for_model", "png_base64": base64::engine::general_purpose::STANDARD.encode(&*png), "max_side": MEDIA_MAX_SIDE, "quality": MEDIA_QUALITY}), &cancel, Duration::from_secs(120))?;
            let jpeg = base64::engine::general_purpose::STANDARD.decode(encoded["jpeg_base64"].as_str().ok_or(Error::Contract)?).map_err(|_| Error::Contract)?;
            let location = if page_no >= 100_000 { "视频画面".to_owned() } else { format!("第{page_no}页") };
            let detail = json!({"display": display, "location": location, "page_no": page_no, "width": encoded["width"], "height": encoded["height"]});
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            let id = crate::handlers::observe::register_native(&vault, grant, page, "image", "image/jpeg", &jpeg, &detail)?;
            out.push(json!({"media_ref": id, "kind": "image", "page_id": page, "display": display, "location": location, "bytes": jpeg.len(), "width": encoded["width"], "height": encoded["height"]}));
        }
        Ok(out)
    }

    fn update_preview(&self, grant: &str, preview: &Value) -> Result<()> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let mut stored = preview.clone();
        if let Some(object) = stored.as_object_mut() {
            for key in ["grant_id", "question_id", "scope_id"] {
                object.remove(key);
            }
        }
        vault.db().execute("UPDATE egress_grants SET preview=? WHERE id=? AND approved_at IS NULL", params![stored.to_string(), grant])?;
        Ok(())
    }

    /// Register an unapproved, single-use grant for one task: destination,
    /// purpose, question, material scope, limits and the excerpts shown.
    #[allow(clippy::too_many_arguments)]
    fn grant_preview(&self, purpose: &str, text: &str, question: &str, scope: &str, matters: usize, connection: Option<&str>, input: Value, excerpts: Excerpts, extra: Value) -> Result<Value> {
        let connection_id = self.connection_for(connection)?;
        let (name, host, model, max_tokens) = {
            let handle = self.vault()?;
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            let loaded = crate::generation::connections::load(&vault, &connection_id)?;
            (loaded.name, loaded.profile.endpoint.display_host(), loaded.profile.model_id, loaded.profile.max_output_tokens)
        };
        let items = match &excerpts {
            // Local-only retrieval for the preview (plain UI root, no generation).
            Excerpts::Retrieval => {
                let envelope = self.run_skill("lvjin-retrieval", json!({"question_id": question, "scope_id": scope, "known_constraint_ids": [], "requested_output": "evidence_only"}), "ui")?;
                match envelope["data_ref"].as_str() {
                    Some(id) => self.result(id)?["payload"]["items"].clone(),
                    None => json!([]),
                }
            }
            Excerpts::Pages(pages) => json!(self.page_excerpts(pages)?),
            Excerpts::Later => json!([]),
        };
        let excerpts: Vec<Value> = items.as_array().cloned().unwrap_or_default().iter().take(EGRESS_MAX_ITEMS as usize).map(|item| json!({
            "evidence_id": item["evidence_id"], "display_name": item["document"]["display_name"], "unit_title": item["unit_title"],
            "pages": item["pages"], "locators": item["locators"], "excerpt": item["exact_text"].as_str().unwrap_or("").chars().take(240).collect::<String>(),
        })).collect();
        let mut preview = json!({"question": text.trim(), "connection": {"id": connection_id, "name": name, "host": host, "model_id": model},
            "purpose": purpose, "excerpts": excerpts, "max_items": EGRESS_MAX_ITEMS, "max_requests": EGRESS_MAX_REQUESTS, "max_output_tokens": max_tokens,
            "validity": "this_task", "matters": matters, "skill_input": input, "native_media": []});
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            preview[key] = value;
        }
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let grant = vault.register("EgressGrantRef", "m2", Some(question))?;
        vault.db().execute(
            "INSERT INTO egress_grants(id,connection_id,host,purpose,scope_id,question_id,max_items,max_requests,preview,created_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
            params![grant, connection_id, host, purpose, scope, question, EGRESS_MAX_ITEMS, EGRESS_MAX_REQUESTS, preview.to_string(), now_rfc3339()],
        )?;
        preview["grant_id"] = json!(grant);
        preview["question_id"] = json!(question);
        preview["scope_id"] = json!(scope);
        Ok(preview)
    }

    /// OCR text of the given pages as preview excerpts (the chunks that will be sent).
    fn page_excerpts(&self, pages: &[String]) -> Result<Vec<Value>> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let mut out = vec![];
        for page in pages {
            let (version, page_no, display): (String, i64, String) = vault.db().query_row(
                "SELECT p.version_id,p.page_no,d.display_name FROM pages p JOIN document_versions v ON v.id=p.version_id JOIN documents d ON d.id=v.document_id WHERE p.id=?",
                [page], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let mut statement = vault.db().prepare("SELECT c.id,c.body,c.spans,c.unit_title FROM chunks c JOIN objects o ON o.id=c.id WHERE c.version_id=? AND o.active=1 ORDER BY c.ordinal")?;
            let rows = statement.query_map([&version], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
            for row in rows {
                let (id, body, spans, unit) = row?;
                let blocks: Vec<i64> = serde_json::from_str::<Vec<Value>>(&spans)?.iter().filter_map(|s| s["block"].as_i64()).collect();
                let mut on_page = false;
                for block in blocks {
                    let owner: Option<String> = vault.db().query_row("SELECT page_id FROM blocks WHERE rowid=?", [block], |r| r.get(0)).optional()?.flatten();
                    on_page |= owner.as_deref() == Some(page.as_str());
                }
                if on_page && out.len() < EGRESS_MAX_ITEMS as usize {
                    out.push(json!({"evidence_id": id, "document": {"display_name": display}, "unit_title": unit, "pages": [page_no], "locators": [], "exact_text": body}));
                }
            }
        }
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

    /// Called only after the native confirmation; then starts the task of the
    /// grant's purpose under a confirmed root that holds generation.request
    /// and this grant. Excluded excerpts and media tighten the grant.
    pub fn answer_start(self: &Arc<Self>, grant: &str, excluded: &[String]) -> Result<String> {
        self.grant_start(grant, excluded, &[])
    }

    pub fn grant_start(self: &Arc<Self>, grant: &str, excluded: &[String], excluded_media: &[String]) -> Result<String> {
        let (purpose, input) = {
            let handle = self.vault()?;
            let vault = handle.lock().map_err(|_| Error::Integrity)?;
            let (purpose, preview): (String, String) = vault.db().query_row("SELECT purpose,preview FROM egress_grants WHERE id=? AND approved_at IS NULL AND root IS NULL AND revoked_at IS NULL", [grant], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
            let mut preview: Value = serde_json::from_str(&preview)?;
            if !excluded.is_empty() || !excluded_media.is_empty() {
                // Excluded excerpts and media cannot be sent in this task.
                preview["excluded"] = json!(excluded);
                preview["excluded_media"] = json!(excluded_media);
                vault.db().execute("UPDATE egress_grants SET preview=? WHERE id=?", params![preview.to_string(), grant])?;
            }
            vault.db().execute("UPDATE egress_grants SET approved_at=? WHERE id=?", params![now_rfc3339(), grant])?;
            (purpose, preview["skill_input"].clone())
        };
        let skill = match purpose.as_str() {
            "answer" | "deep_answer" => "lvjin-retrieval",
            "contract_review" => "contract-review",
            "loan_analysis" => "loan-analysis",
            "budget_analysis" => "budget-analysis",
            "image_review" => "image-review",
            "audio_review" => "audio-review",
            "video_review" => "video-review",
            _ => return Err(Error::Integrity),
        };
        let (handle, invocation) = self.begin_with(skill, &input, "confirm", Some(grant))?;
        self.services.log.event("egress.approved", &[("purpose", crate::logging::Field::Status(purpose.clone()))]);
        self.spawn(skill, input, handle, invocation)
    }

    pub fn answer_cancel_preview(&self, grant: &str) -> Result<()> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        let revoked = vault.db().execute("UPDATE egress_grants SET revoked_at=? WHERE id=? AND approved_at IS NULL", params![now_rfc3339(), grant])?;
        if revoked == 1 {
            // The images prepared for the preview were never sent; they go now.
            handlers::observe::release_native(&vault, grant)?;
        }
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

/// Inspection (and simulated crash states) for the synthetic tests and
/// benchmarks. Compiled only with `m0-harness`; production builds have no raw
/// SQL entry point.
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

    /// Simulate on-disk states that only a process crash leaves behind.
    pub fn harness_execute(&self, sql: &str, args: &[&str]) -> Result<usize> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        Ok(vault.db().execute(sql, rusqlite::params_from_iter(args.iter()))?)
    }

    /// Encrypted object files currently registered in the unlocked vault.
    pub fn harness_blob_files(&self) -> Result<Vec<(String, PathBuf)>> {
        let handle = self.vault()?;
        let vault = handle.lock().map_err(|_| Error::Integrity)?;
        vault.blob_files()
    }

    /// Lock-policy tests: how long the user has been idle, when a stand-in
    /// running task last reported progress (it is added when `stalled` is
    /// given) and since when the auto-lock has been deferred.
    pub fn harness_lock_clock(&self, idle: Duration, stalled: Option<Duration>, deferred: Option<Duration>) -> Result<()> {
        let now = Instant::now();
        let back = |ago: Duration| now.checked_sub(ago).ok_or(Error::Contract);
        *self.activity.lock().map_err(|_| Error::Integrity)? = back(idle)?;
        if let Some(stalled) = stalled {
            let moved = back(stalled)?;
            let mut tasks = self.tasks.lock().map_err(|_| Error::Integrity)?;
            let task = tasks.entry("task_harness_running".into()).or_insert_with(|| Task {
                state: TaskState {
                    task_id: "task_harness_running".into(), skill: "document-import".into(), root: String::new(), status: "running".into(), progress: None, envelope: None,
                    started_at: now_rfc3339(), retryable: false,
                },
                cancel: Arc::new(AtomicBool::new(false)),
                thread: None,
                moved,
                retry: None,
            });
            task.moved = moved;
        }
        *self.deferred.lock().map_err(|_| Error::Integrity)? = match deferred {
            Some(ago) => Some(back(ago)?),
            None => None,
        };
        Ok(())
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

fn interrupted_run(handle: &VaultHandle, root: &str) -> Option<String> {
    let vault = handle.lock().ok()?;
    vault.db().query_row("SELECT id FROM flow_runs WHERE root=? AND status='interrupted' ORDER BY updated_at DESC", [root], |r| r.get(0)).optional().ok()?
}

fn wav_from_f32(bytes: &[u8]) -> Vec<u8> {
    let samples: Vec<i16> = bytes
        .chunks_exact(4)
        .map(|c| (f32::from_le_bytes([c[0], c[1], c[2], c[3]]).clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    crate::generation::probe::wav_pcm16(&samples, 16_000)
}
