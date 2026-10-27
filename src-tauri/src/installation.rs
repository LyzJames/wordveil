//! Compile-anchored offline installation inventory. Paths originate here, never
//! in a Skill or model request. The enclosing resource lock anchors this file too.
use crate::{Error,Result,model_pool::ModelPool,worker_process::{ArtifactFile,CpuArtifact,WorkerInstallation,verify_files}};
use serde::Deserialize;
use sha2::{Digest,Sha256};
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub schema_version:String,
    pub release_version:String,
    pub cpu_only:bool,
    pub worker_directory:String,
    pub artifacts:Vec<CpuArtifact>,
    pub worker_files:Vec<ArtifactFile>,
    pub runtime_files:Vec<ArtifactFile>,
}
impl Installation {
    pub fn load(root:&Path)->Result<Self> {
        let expected=include_bytes!("../../resources/runtime/installation.json");
        let bytes=std::fs::read(root.join("resources/runtime/installation.json"))?;
        if Sha256::digest(&bytes)!=Sha256::digest(expected) {return Err(Error::Integrity);}
        let value:Self=serde_json::from_slice(&bytes)?;
        let identities:std::collections::BTreeSet<_>=value.artifacts.iter().map(|v|v.key.model.as_str()).collect();
        let fixed:std::collections::BTreeSet<_>=["embedding","privacy-has","ocr-det","uvdoc","textline-ori","ocr-rec","doc-ori","asr"].into_iter().collect();
        if value.schema_version!="lvjin.installation/1" || value.release_version!=crate::RELEASE_VERSION || !value.cpu_only || value.artifacts.iter().any(|a|a.key.device!="cpu") || value.artifacts.len()!=8 || identities!=fixed || value.worker_directory!="runtimes/worker" || value.runtime_files.is_empty(){return Err(Error::Integrity);}
        Ok(value)
    }
    pub fn pool(self,root:&Path,work:&Path)->Result<ModelPool> {
        verify_files(root,&self.runtime_files)?;
        WorkerInstallation::new(root,Path::new(&self.worker_directory),work,self.artifacts,self.worker_files)?.pool()
    }
}
