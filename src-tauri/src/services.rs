//! Process-wide trusted services: installation layout, model pool, local worker,
//! media tools and the redacted event log. None of these hold vault keys.
use crate::{
    Error, Result,
    index::EmbeddingProfile,
    local_worker::LocalWorker,
    logging::EventLog,
    media::MediaTools,
    model_pool::{ModelKey, ModelLease, ModelPool, ResidencyPolicy, installed_memory_bytes},
    isolation::SpawnPolicy,
    worker_process::{ArtifactFile, CpuArtifact, Launcher, WorkerInstallation},
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};

/// Global limit on concurrent requests to generation services (design 7.3:
/// at most two in flight across all roles and roots).
pub const GENERATION_CONCURRENCY: u32 = 2;
const ORCHESTRATOR_MEMORY: usize = 1024 * 1024 * 1024;

/// A counting semaphore whose waiters keep honouring cancellation.
pub struct Slots {
    used: Mutex<u32>,
    freed: Condvar,
    limit: u32,
}

pub struct SlotGuard<'a>(&'a Slots);

impl Slots {
    pub fn new(limit: u32) -> Self {
        Self { used: Mutex::new(0), freed: Condvar::new(), limit }
    }
    pub fn acquire(&self, cancellation: &AtomicBool) -> Result<SlotGuard<'_>> {
        let mut used = self.used.lock().map_err(|_| Error::Integrity)?;
        while *used >= self.limit {
            if cancellation.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            used = self.freed.wait_timeout(used, Duration::from_millis(50)).map_err(|_| Error::Integrity)?.0;
        }
        *used += 1;
        Ok(SlotGuard(self))
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut used) = self.0.used.lock() {
            *used = used.saturating_sub(1);
        }
        self.0.freed.notify_one();
    }
}

#[derive(Clone, Debug)]
pub struct EmbeddingSpace {
    pub id: String,
    pub profile: EmbeddingProfile,
}

#[derive(Deserialize)]
struct SpaceFile {
    profile: EmbeddingProfile,
    embedding_space_id: String,
}

/// Where trusted resources live. Production paths come only from the installed
/// tree beside the executable; development paths are compiled only with
/// `dev-worker` and point at verified M0 source artifacts under docs.
#[derive(Clone)]
pub struct Layout {
    pub install_root: PathBuf,
    pub resources: PathBuf,
    pub data_root: PathBuf,
    pub ffmpeg_bin: PathBuf,
    pub tokenizer: PathBuf,
    pub launcher: Launcher,
    pub artifacts: Vec<CpuArtifact>,
    pub worker_files: Vec<ArtifactFile>,
    pub space: EmbeddingSpace,
    pub production: bool,
}

fn load_space(resources: &Path) -> Result<EmbeddingSpace> {
    let file: SpaceFile = serde_json::from_slice(&std::fs::read(resources.join("models/embedding-space.json"))?)?;
    if file.profile.space_id()? != file.embedding_space_id {
        return Err(Error::Integrity);
    }
    Ok(EmbeddingSpace { id: file.embedding_space_id, profile: file.profile })
}

impl Layout {
    /// Installed application: `<root>/lvjin.exe`, resources, runtimes, data-root.txt.
    pub fn production(install_root: &Path, data_root: &Path) -> Result<Self> {
        let installation = crate::installation::Installation::load(install_root)?;
        let resources = install_root.join("resources");
        let space = load_space(&resources)?;
        let embedding = installation.artifacts.iter().find(|a| a.key.model == "embedding").ok_or(Error::Integrity)?;
        if embedding.key.artifact != space.profile.artifact_sha256 {
            return Err(Error::Integrity);
        }
        Ok(Self {
            tokenizer: install_root.join(&embedding.directory).join("tokenizer.json"),
            launcher: Launcher::Frozen { directory: install_root.join(&installation.worker_directory) },
            ffmpeg_bin: install_root.join("runtimes/ffmpeg"),
            artifacts: installation.artifacts,
            worker_files: installation.worker_files,
            install_root: install_root.to_path_buf(),
            resources,
            data_root: data_root.to_path_buf(),
            space,
            production: true,
        })
    }

    /// Development/test layout: app sources, M0 source artifacts and the docs
    /// FFmpeg build. Artifact keys and file hashes equal the production ones.
    #[cfg(feature = "dev-worker")]
    pub fn development(docs: &Path, app: &Path, data_root: &Path) -> Result<Self> {
        #[derive(Deserialize)]
        struct DevInstall {
            artifacts: Vec<CpuArtifact>,
        }
        let resources = app.join("resources");
        let space = load_space(&resources)?;
        let dev: DevInstall = serde_json::from_slice(&std::fs::read(docs.join("fixtures/m1/dev-installation.json"))?)?;
        let embedding = dev.artifacts.iter().find(|a| a.key.model == "embedding").ok_or(Error::Integrity)?;
        if embedding.key.artifact != space.profile.artifact_sha256 {
            return Err(Error::Integrity);
        }
        let python = docs.join("tooling/env/python312");
        Ok(Self {
            tokenizer: docs.join(&embedding.directory).join("tokenizer.json"),
            launcher: Launcher::Source {
                python: python.join("python-utf8.exe"),
                base: python.clone(),
                site: docs.join("tooling/env/m0-py/Lib/site-packages"),
                app: app.to_path_buf(),
            },
            ffmpeg_bin: docs.join("build/ffmpeg-dist/bin"),
            artifacts: dev.artifacts,
            worker_files: vec![],
            install_root: docs.to_path_buf(),
            resources,
            data_root: data_root.to_path_buf(),
            space,
            production: false,
        })
    }

    pub fn vaults_root(&self) -> PathBuf {
        self.data_root.join("vaults")
    }
    pub fn temp_root(&self) -> PathBuf {
        self.data_root.join("temp")
    }
    pub fn logs_root(&self) -> PathBuf {
        self.data_root.join("logs")
    }
}

pub struct Services {
    pub layout: Layout,
    pool: Mutex<Option<ModelPool>>,
    pub local: LocalWorker,
    pub media: MediaTools,
    pub log: EventLog,
    pub residency: ResidencyPolicy,
    pub generation_slots: Slots,
}

impl Services {
    pub fn new(layout: Layout) -> Result<Self> {
        // Remove plaintext scratch left behind by a crash before anything runs.
        let temp = layout.temp_root();
        if temp.is_dir() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        std::fs::create_dir_all(temp.join("workers"))?;
        std::fs::create_dir_all(temp.join("media"))?;
        std::fs::create_dir_all(layout.vaults_root())?;
        let tokenizer = layout.tokenizer.canonicalize().ok();
        Ok(Self {
            local: LocalWorker::new(layout.launcher.clone(), tokenizer, temp.join("workers")),
            media: MediaTools { bin: layout.ffmpeg_bin.clone(), work_root: temp.join("media") },
            log: EventLog::new(layout.logs_root()),
            residency: ResidencyPolicy::for_installed_memory(installed_memory_bytes()),
            generation_slots: Slots::new(GENERATION_CONCURRENCY),
            pool: Mutex::new(None),
            layout,
        })
    }

    pub fn pool(&self) -> Result<ModelPool> {
        let mut guard = self.pool.lock().map_err(|_| Error::Integrity)?;
        if let Some(pool) = guard.as_ref() {
            return Ok(pool.clone());
        }
        let installation = WorkerInstallation::with_launcher(
            &self.layout.install_root,
            self.layout.launcher.clone(),
            &self.layout.temp_root().join("workers"),
            self.layout.artifacts.clone(),
            self.layout.worker_files.clone(),
        )?;
        let pool = installation.pool_with(self.residency.clone())?;
        *guard = Some(pool.clone());
        Ok(pool)
    }

    pub fn key(&self, model: &str) -> Result<ModelKey> {
        self.layout.artifacts.iter().find(|a| a.key.model == model).map(|a| a.key.clone()).ok_or(Error::Integrity)
    }

    pub fn lease(&self, model: &str, cancellation: &AtomicBool) -> Result<ModelLease> {
        let key = self.key(model)?;
        self.pool()?.acquire(&key, cancellation, Instant::now() + Duration::from_secs(600))
    }

    /// One fixed-model inference through the shared owner (singleflight, one
    /// heavy stage). The lease is released afterwards (idle TTL applies).
    pub fn predict(&self, model: &str, request: &Value, cancellation: &AtomicBool) -> Result<Value> {
        let lease = self.lease(model, cancellation)?;
        lease.predict(request, cancellation)
    }

    /// A fresh private scratch directory for one isolated process.
    pub fn scratch(&self, prefix: &str) -> Result<PathBuf> {
        let path = self.layout.temp_root().join("workers").join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    /// Spawn policy of one orchestrator process: the worker bundle only, no
    /// model, tokenizer or business path, no network (AppContainer).
    pub fn orchestrator_policy(&self, scratch: PathBuf) -> Result<SpawnPolicy> {
        // Synthetic tests only (never compiled into production): a scripted,
        // possibly hostile orchestrator `script.py|scenario`, run isolated alike.
        #[cfg(all(feature = "m0-harness", feature = "dev-worker"))]
        if let (Some(spec), Launcher::Source { python, base, .. }) = (std::env::var_os("LVJIN_TEST_ORCHESTRATOR"), &self.layout.launcher) {
            let spec = spec.to_string_lossy().to_string();
            let (script, scenario) = spec.split_once('|').ok_or(Error::Contract)?;
            let script = PathBuf::from(script);
            let directory = script.parent().ok_or(Error::Contract)?.to_path_buf();
            return Ok(SpawnPolicy {
                executable: python.clone(),
                arguments: vec!["-I".into(), "-B".into(), "-u".into(), script.to_string_lossy().to_string(), scenario.into()],
                read_only: vec![base.clone(), directory],
                scratch,
                memory_limit: ORCHESTRATOR_MEMORY,
            });
        }
        self.layout.launcher.policy(vec!["--service".into(), "orchestrator".into()], vec![], scratch, ORCHESTRATOR_MEMORY)
    }

    /// Lock/exit: stop every sensitive worker so no plaintext stays resident.
    pub fn stop_sensitive(&self) {
        self.local.stop();
        if let Ok(guard) = self.pool.lock() {
            if let Some(pool) = guard.as_ref() {
                let _ = pool.evict_idle();
            }
        }
    }
}
