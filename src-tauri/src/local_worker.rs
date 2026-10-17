//! The non-model local service worker (parsing, OCR geometry, chunking,
//! extraction, finance) in the same no-network AppContainer as model workers.
//!
//! Document bytes are uploaded in bounded chunks over the inherited pipe; the
//! worker never receives a path, URL, key or credential. Any transport failure
//! terminates the process; the next request starts a fresh one.
use crate::{
    Error, Result,
    isolation::IsolatedProcess,
    worker_process::Launcher,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};

const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;
const MEMORY_LIMIT: usize = 3 * 1024 * 1024 * 1024;

pub struct LocalWorker {
    launcher: Launcher,
    tokenizer: Option<PathBuf>,
    work_root: PathBuf,
    state: Mutex<Option<(IsolatedProcess, PathBuf)>>,
}

pub struct Upload<'a> {
    worker: &'a LocalWorker,
    pub id: String,
}

impl Drop for Upload<'_> {
    fn drop(&mut self) {
        let _ = self.worker.call(&json!({"op": "upload_release", "upload_id": self.id}), &AtomicBool::new(false), Duration::from_secs(30));
    }
}

impl LocalWorker {
    pub fn new(launcher: Launcher, tokenizer: Option<PathBuf>, work_root: PathBuf) -> Self {
        Self { launcher, tokenizer, work_root, state: Mutex::new(None) }
    }

    fn spawn(&self) -> Result<(IsolatedProcess, PathBuf)> {
        let scratch = self.work_root.join(format!("local-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&scratch)?;
        let mut arguments = vec!["--service".to_string(), "local".to_string()];
        let mut read_only = vec![];
        if let Some(tokenizer) = &self.tokenizer {
            let text = tokenizer.to_str().ok_or(Error::Integrity)?;
            arguments.extend(["--tokenizer".into(), text.strip_prefix("\\\\?\\").unwrap_or(text).to_owned()]);
            read_only.push(tokenizer.clone());
        }
        let policy = self.launcher.policy(arguments, read_only, scratch.clone(), MEMORY_LIMIT)?;
        let mut process = IsolatedProcess::spawn(policy)?;
        let ready = match process.receive(&AtomicBool::new(false), Instant::now() + Duration::from_secs(120)) {
            Ok(value) => value,
            Err(_) => {
                let _ = process.terminate();
                drop(process);
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(Error::Worker("local_worker_start_failed".into()));
            }
        };
        if ready.get("event").and_then(Value::as_str) != Some("ready") || ready.get("service").and_then(Value::as_str) != Some("local") {
            process.terminate()?;
            return Err(Error::Integrity);
        }
        Ok((process, scratch))
    }

    /// One request/response exchange. Service errors are stable codes only.
    pub fn call(&self, request: &Value, cancellation: &AtomicBool, timeout: Duration) -> Result<Value> {
        let mut state = self.state.lock().map_err(|_| Error::Integrity)?;
        if state.is_none() {
            *state = Some(self.spawn()?);
        }
        let deadline = Instant::now() + timeout;
        let exchange = (|| -> Result<Value> {
            let (process, _) = state.as_mut().ok_or(Error::Integrity)?;
            process.send(request, cancellation, deadline)?;
            process.receive(cancellation, deadline)
        })();
        match exchange {
            Ok(response) => match response.get("event").and_then(Value::as_str) {
                Some("result") => response.get("data").cloned().ok_or(Error::Integrity),
                Some("error") => Err(Error::Worker(crate::sanitize_code(response.get("code").and_then(Value::as_str).unwrap_or("")))),
                _ => {
                    Self::discard(&mut state);
                    Err(Error::Contract)
                }
            },
            Err(error) => {
                Self::discard(&mut state);
                Err(error)
            }
        }
    }

    fn discard(state: &mut Option<(IsolatedProcess, PathBuf)>) {
        if let Some((mut process, scratch)) = state.take() {
            let _ = process.terminate();
            drop(process);
            let _ = std::fs::remove_dir_all(scratch);
        }
    }

    /// Upload document bytes; the returned guard releases worker memory on drop.
    pub fn upload(&self, data: &[u8], cancellation: &AtomicBool) -> Result<Upload<'_>> {
        let id = format!("u{}", uuid::Uuid::new_v4().simple());
        self.call(&json!({"op": "upload_begin", "upload_id": id, "bytes": data.len()}), cancellation, Duration::from_secs(30))?;
        let upload = Upload { worker: self, id };
        for (index, chunk) in data.chunks(UPLOAD_CHUNK).enumerate() {
            self.call(
                &json!({"op": "upload_chunk", "upload_id": upload.id, "offset": index * UPLOAD_CHUNK, "data_b64": STANDARD.encode(chunk)}),
                cancellation,
                Duration::from_secs(60),
            )?;
        }
        if data.is_empty() {
            self.call(&json!({"op": "upload_chunk", "upload_id": upload.id, "offset": 0, "data_b64": ""}), cancellation, Duration::from_secs(30))?;
        }
        Ok(upload)
    }

    pub fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            Self::discard(&mut state);
        }
    }

    pub fn running(&self) -> bool {
        self.state.lock().map(|state| state.is_some()).unwrap_or(false)
    }
}

impl Drop for LocalWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
