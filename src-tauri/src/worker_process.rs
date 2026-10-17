//! A model owner is a real AppContainer process. A failed stage is terminated and
//! waited before the pool can admit a replacement; there is no automatic retry.
use crate::{Error,Result,isolation::{IsolatedProcess,SpawnPolicy},model_pool::{AllowedArtifact,ManagedModel,ModelKey,ModelPool}};
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

pub struct WorkerInstallation {
    root:PathBuf,worker_directory:PathBuf,work_root:PathBuf,
    artifacts:BTreeMap<ModelKey,CpuArtifact>,worker_files:Vec<ArtifactFile>,
}

impl WorkerInstallation {
    /// The caller verifies the enclosing manifest against the core's build anchor.
    /// This additionally enforces exact CPU profiles and verifies physical files.
    pub fn new(root:&Path,worker_relative:&Path,work_root:&Path,artifacts:Vec<CpuArtifact>,worker_files:Vec<ArtifactFile>)->Result<Self> {
        let root=root.canonicalize()?;
        let worker_directory=root.join(worker_relative).canonicalize()?;
        if !worker_directory.starts_with(&root) || worker_files.is_empty() { return Err(Error::Integrity); }
        std::fs::create_dir_all(work_root)?;
        let work_root=work_root.canonicalize()?;
        let mut selected=BTreeMap::new();
        for artifact in artifacts {
            if artifact.key.device!="cpu" || artifact.key.profile.is_empty() || artifact.files.is_empty() || !matches!(artifact.key.model.as_str(),"embedding"|"reranker"|"grounding"|"asr"|"ocr-det"|"ocr-rec"|"uvdoc"|"textline-ori"|"doc-ori") { return Err(Error::Integrity); }
            let directory=root.join(&artifact.directory).canonicalize()?;
            if !directory.starts_with(&root) { return Err(Error::Integrity); }
            if selected.insert(artifact.key.clone(),artifact).is_some() { return Err(Error::Integrity); }
        }
        Ok(Self { root,worker_directory,work_root,artifacts:selected,worker_files })
    }

    pub fn pool(self)->Result<ModelPool> {
        // Validate the executable and all DLL/Python data files before granting
        // the worker read access. Worker code never decides what is trusted.
        verify_files(&self.worker_directory,&self.worker_files)?;
        let records=self.artifacts.values().collect::<Vec<_>>();
        let identity=format!("{:x}",Sha256::digest(serde_json::to_vec(&(self.root.to_str().ok_or(Error::Integrity)?,self.worker_directory.to_str().ok_or(Error::Integrity)?,self.work_root.to_str().ok_or(Error::Integrity)?,records,&self.worker_files))?));
        let allowed=self.artifacts.values().map(|item|AllowedArtifact { model:item.key.model.clone(),revision:item.key.revision.clone(),artifact:item.key.artifact.clone(),verified_keys:vec![item.key.clone()] }).collect();
        let installation=Arc::new(self);
        ModelPool::production(allowed,identity,Box::new(move |key,cancellation,deadline|installation.load(key,cancellation,deadline)))
    }

    fn load(&self,key:&ModelKey,cancellation:&AtomicBool,deadline:Instant)->Result<Box<dyn ManagedModel>> {
        let artifact=self.artifacts.get(key).ok_or(Error::Denied)?;
        let directory=self.root.join(&artifact.directory).canonicalize()?;
        if !directory.starts_with(&self.root) { return Err(Error::Integrity); }
        verify_files(&directory,&artifact.files)?;
        let scratch=self.work_root.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&scratch)?;
        let argument=directory.to_str().ok_or(Error::Integrity)?.strip_prefix("\\\\?\\").unwrap_or(directory.to_str().ok_or(Error::Integrity)?).to_owned();
        let mut process=IsolatedProcess::spawn(SpawnPolicy {
            executable:self.worker_directory.join("lvjin-worker.exe"),
            arguments:vec!["--model".into(),key.model.clone(),"--artifact".into(),argument],
            read_only:vec![self.worker_directory.clone(),directory],scratch,
            memory_limit:8usize*1024*1024*1024,
        })?;
        let ready=process.receive(cancellation,deadline)?;
        if ready.get("event").and_then(Value::as_str)!=Some("ready") || ready.get("model_id").and_then(Value::as_str)!=Some(&key.model) {
            process.terminate()?; return Err(Error::Integrity);
        }
        Ok(Box::new(CpuWorker { process,model_id:key.model.clone(),stopped:false }))
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

struct CpuWorker { process:IsolatedProcess,model_id:String,stopped:bool }
impl ManagedModel for CpuWorker {
    fn predict(&mut self,input:&Value,cancellation:&AtomicBool)->Result<Value> {
        if self.stopped { return Err(Error::Integrity); }
        if input.get("kind").and_then(Value::as_str)!=Some(self.model_id.as_str()) { return Err(Error::Contract); }
        let deadline=Instant::now()+Duration::from_secs(120);
        self.process.send(input,cancellation,deadline)?;
        let response=self.process.receive(cancellation,deadline)?;
        if response.get("event").and_then(Value::as_str)!=Some("result") { return Err(Error::Contract); }
        response.get("data").cloned().ok_or(Error::Integrity)
    }
    fn shutdown(&mut self)->Result<()> { self.process.terminate()?;self.stopped=true;Ok(()) }
    fn process_id(&self)->Option<u32> { Some(self.process.pid) }
    fn memory(&self)->Option<crate::model_pool::ModelMemory> {self.process.memory().ok()}
}
