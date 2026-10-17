//! A model owner is a real AppContainer process. A failed stage is terminated and
//! waited before the pool can admit a replacement; there is no automatic retry.
use crate::{Error,Result,isolation::{IsolatedProcess,SpawnPolicy},model_pool::{AllowedArtifact,ManagedModel,ModelKey,ModelPool,ResidencyPolicy}};
use serde::{Deserialize,Serialize};
use serde_json::Value;
use sha2::{Digest,Sha256};
use std::{collections::BTreeMap,fs::File,io::{Read,BufReader},path::{Path,PathBuf},sync::{Arc,atomic::AtomicBool},time::{Duration,Instant}};

#[derive(Clone,Deserialize,Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFile { pub path:String,pub bytes:u64,pub sha256:String }
#[derive(Clone,Deserialize,Serialize)]
#[serde(deny_unknown_fields)]
pub struct CpuArtifact {
    pub key:ModelKey,
    /// Relative to the trusted installation root, never from a business payload.
    pub directory:String,
    pub files:Vec<ArtifactFile>,
}

/// How an isolated worker process is started. Production uses only the frozen,
/// hash-verified worker. The source launcher exists only in dev/test builds.
#[derive(Clone)]
pub enum Launcher {
    Frozen { directory:PathBuf },
    #[cfg(feature="dev-worker")]
    Source { python:PathBuf, base:PathBuf, site:PathBuf, app:PathBuf },
}

#[cfg(feature="dev-worker")]
const SOURCE_BOOT:&str="import sys;sys.path[:0]=[sys.argv.pop(1),sys.argv.pop(1)];from agent_core.worker import main;raise SystemExit(main())";

impl Launcher {
    pub fn policy(&self,arguments:Vec<String>,mut read_only:Vec<PathBuf>,scratch:PathBuf,memory_limit:usize)->Result<SpawnPolicy> {
        match self {
            Launcher::Frozen{directory}=>{
                read_only.insert(0,directory.clone());
                Ok(SpawnPolicy{executable:directory.join("lvjin-worker.exe"),arguments,read_only,scratch,memory_limit})
            }
            #[cfg(feature="dev-worker")]
            Launcher::Source{python,base,site,app}=>{
                read_only.extend([base.clone(),site.clone(),app.clone()]);
                let mut all=vec!["-I".into(),"-B".into(),"-u".into(),"-c".into(),SOURCE_BOOT.into(),strip(site)?,strip(app)?];
                all.extend(arguments);
                Ok(SpawnPolicy{executable:python.clone(),arguments:all,read_only,scratch,memory_limit})
            }
        }
    }
    pub fn is_frozen(&self)->bool { matches!(self,Launcher::Frozen{..}) }
}

fn strip(path:&Path)->Result<String> {
    let text=path.to_str().ok_or(Error::Integrity)?;
    Ok(text.strip_prefix("\\\\?\\").unwrap_or(text).to_owned())
}

pub struct WorkerInstallation {
    root:PathBuf,launcher:Launcher,work_root:PathBuf,
    artifacts:BTreeMap<ModelKey,CpuArtifact>,worker_files:Vec<ArtifactFile>,
}

impl WorkerInstallation {
    /// The caller verifies the enclosing manifest against the core's build anchor.
    /// This additionally enforces exact CPU profiles and verifies physical files.
    pub fn new(root:&Path,worker_relative:&Path,work_root:&Path,artifacts:Vec<CpuArtifact>,worker_files:Vec<ArtifactFile>)->Result<Self> {
        let root=root.canonicalize()?;
        let worker_directory=root.join(worker_relative).canonicalize()?;
        if !worker_directory.starts_with(&root) || worker_files.is_empty() { return Err(Error::Integrity); }
        Self::with_launcher(&root,Launcher::Frozen{directory:worker_directory},work_root,artifacts,worker_files)
    }

    pub fn with_launcher(root:&Path,launcher:Launcher,work_root:&Path,artifacts:Vec<CpuArtifact>,worker_files:Vec<ArtifactFile>)->Result<Self> {
        let root=root.canonicalize()?;
        if launcher.is_frozen() && worker_files.is_empty() { return Err(Error::Integrity); }
        std::fs::create_dir_all(work_root)?;
        let work_root=work_root.canonicalize()?;
        let mut selected=BTreeMap::new();
        for artifact in artifacts {
            if artifact.key.device!="cpu" || artifact.key.profile.is_empty() || artifact.files.is_empty() || !matches!(artifact.key.model.as_str(),"embedding"|"reranker"|"grounding"|"asr"|"ocr-det"|"ocr-rec"|"uvdoc"|"textline-ori"|"doc-ori") { return Err(Error::Integrity); }
            let directory=root.join(&artifact.directory).canonicalize()?;
            if !directory.starts_with(&root) { return Err(Error::Integrity); }
            if selected.insert(artifact.key.clone(),artifact).is_some() { return Err(Error::Integrity); }
        }
        Ok(Self { root,launcher,work_root,artifacts:selected,worker_files })
    }

    pub fn pool(self)->Result<ModelPool> { self.pool_with(ResidencyPolicy::default()) }

    pub fn pool_with(self,residency:ResidencyPolicy)->Result<ModelPool> {
        // Validate the executable and all DLL/Python data files before granting
        // the worker read access. Worker code never decides what is trusted.
        if let Launcher::Frozen{directory}=&self.launcher { verify_files(directory,&self.worker_files)?; }
        let records=self.artifacts.values().collect::<Vec<_>>();
        let launcher_identity=match &self.launcher { Launcher::Frozen{directory}=>directory.to_str().ok_or(Error::Integrity)?.to_owned(), #[cfg(feature="dev-worker")] Launcher::Source{app,..}=>format!("source:{}",app.display()) };
        // The identity names what runs (installation root, launcher, artifacts,
        // worker files). The scratch root is where it runs: a second data root in
        // the same process (restore onto a new location) joins the existing
        // coordinator instead of creating a parallel owner.
        let identity=format!("{:x}",Sha256::digest(serde_json::to_vec(&(self.root.to_str().ok_or(Error::Integrity)?,launcher_identity,records,&self.worker_files))?));
        let allowed=self.artifacts.values().map(|item|AllowedArtifact { model:item.key.model.clone(),revision:item.key.revision.clone(),artifact:item.key.artifact.clone(),verified_keys:vec![item.key.clone()] }).collect();
        let installation=Arc::new(self);
        ModelPool::production(allowed,identity,residency,Box::new(move |key,cancellation,deadline|installation.load(key,cancellation,deadline)))
    }

    pub fn artifact(&self,model:&str)->Option<&CpuArtifact> { self.artifacts.values().find(|a|a.key.model==model) }

    fn load(&self,key:&ModelKey,cancellation:&AtomicBool,deadline:Instant)->Result<Box<dyn ManagedModel>> {
        let artifact=self.artifacts.get(key).ok_or(Error::Denied)?;
        let directory=self.root.join(&artifact.directory).canonicalize()?;
        if !directory.starts_with(&self.root) { return Err(Error::Integrity); }
        verify_files(&directory,&artifact.files)?;
        let scratch=self.work_root.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&scratch)?;
        let argument=strip(&directory)?;
        let policy=self.launcher.policy(vec!["--model".into(),key.model.clone(),"--artifact".into(),argument],vec![directory],scratch.clone(),8usize*1024*1024*1024)?;
        let mut process=IsolatedProcess::spawn(policy)?;
        let ready=process.receive(cancellation,deadline)?;
        if ready.get("event").and_then(Value::as_str)!=Some("ready") || ready.get("model_id").and_then(Value::as_str)!=Some(&key.model) {
            process.terminate()?; return Err(Error::Integrity);
        }
        Ok(Box::new(CpuWorker { process,model_id:key.model.clone(),stopped:false,scratch }))
    }
}

pub fn verify_files(root:&Path,files:&[ArtifactFile])->Result<()> {
    let root=root.canonicalize()?;
    for expected in files {
        let relative=Path::new(&expected.path);
        if relative.is_absolute() || relative.components().any(|part|!matches!(part,std::path::Component::Normal(_))) { return Err(Error::Integrity); }
        let path=root.join(relative).canonicalize()?;
        if !path.starts_with(&root) { return Err(Error::Integrity); }
        let stream=File::open(path)?;
        if stream.metadata()?.len()!=expected.bytes { return Err(Error::Integrity); }
        let mut reader=BufReader::new(stream);let mut digest=Sha256::new();let mut buffer=[0u8;64*1024];
        loop { let read=reader.read(&mut buffer)?;if read==0 { break; } digest.update(&buffer[..read]); }
        if format!("{:x}",digest.finalize())!=expected.sha256 { return Err(Error::Integrity); }
    }
    Ok(())
}

struct CpuWorker { process:IsolatedProcess,model_id:String,stopped:bool,scratch:PathBuf }
impl ManagedModel for CpuWorker {
    fn predict(&mut self,input:&Value,cancellation:&AtomicBool)->Result<Value> {
        if self.stopped { return Err(Error::Integrity); }
        if input.get("kind").and_then(Value::as_str)!=Some(self.model_id.as_str()) { return Err(Error::Contract); }
        // Batched OCR/ASR stages can legitimately take minutes on 4 CPU threads.
        let deadline=Instant::now()+Duration::from_secs(600);
        self.process.send(input,cancellation,deadline)?;
        let response=self.process.receive(cancellation,deadline)?;
        match response.get("event").and_then(Value::as_str) {
            Some("result")=>response.get("data").cloned().ok_or(Error::Integrity),
            Some("error")=>Err(Error::Worker(crate::sanitize_code(response.get("code").and_then(Value::as_str).unwrap_or("")))),
            _=>Err(Error::Contract),
        }
    }
    fn shutdown(&mut self)->Result<()> {
        self.process.terminate()?;self.stopped=true;
        // The private scratch (AppContainer temp, vendor caches) goes with the owner.
        let _=std::fs::remove_dir_all(&self.scratch);
        Ok(())
    }
    fn process_id(&self)->Option<u32> { Some(self.process.pid) }
    fn memory(&self)->Option<crate::model_pool::ModelMemory> {self.process.memory().ok()}
}
