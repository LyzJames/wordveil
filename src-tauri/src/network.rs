//! Bounded, explicitly approved external requests. No redirect, remote code,
//! ambient credential, automatic retry, or remote resource is executed here.
use crate::{Error, Result, generation::transport::{self, Endpoint, HttpOptions}, services::{SlotGuard, Slots}};
use serde_json::Value;
use std::{collections::HashMap, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}, mpsc}, time::{Duration, Instant}};
use zeroize::Zeroizing;

pub const MAX_BODY: usize = 2 * 1024 * 1024;
pub const MAX_TEXT: usize = 128_000;
pub const OFFICIAL_DOMAINS: &[&str] = &["gov.cn", "npc.gov.cn", "court.gov.cn", "pbc.gov.cn", "nfra.gov.cn", "samr.gov.cn", "csrc.gov.cn"];

pub fn public_url(text: &str, domains: &[String]) -> Result<String> {
    let mut url = url::Url::parse(text).map_err(|_| Error::NeedsInput("public_url"))?;
    let host = url.host_str().ok_or(Error::Contract)?.to_ascii_lowercase();
    if text.chars().count() > 8192 || text.chars().any(char::is_control) || url.scheme() != "https" || !url.username().is_empty() || url.password().is_some()
        || url.port().is_some_and(|p| p != 443) || host.parse::<std::net::IpAddr>().is_ok()
        || host == "localhost" || host.ends_with(".localhost") || !domains.iter().any(|d| host == *d || host.ends_with(&format!(".{d}"))) {
        return Err(Error::Denied);
    }
    url.set_fragment(None);
    Ok(url.into())
}

pub fn checked_domains(domains: &[String]) -> Result<Vec<String>> {
    if domains.is_empty() || domains.len() > 20 { return Err(Error::Contract); }
    domains.iter().map(|domain| {
        let d = domain.trim().to_ascii_lowercase();
        if d.is_empty() || d.len() > 253 || !d.contains('.') || d.parse::<std::net::IpAddr>().is_ok()
            || !d.split('.').all(|p| !p.is_empty() && p.len()<=63 && !p.starts_with('-') && !p.ends_with('-') && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
            return Err(Error::Contract);
        }
        Ok(d)
    }).collect()
}

/// Conservative app-local pacing (other applications can still consume the
/// account quota): 4/s per Search/Fetch lane and 1/s per Fetch target host.
#[derive(Default)]
pub struct RateLimits(Mutex<HashMap<String, Instant>>);
impl RateLimits {
    pub fn wait(&self, lane: &str, domain: Option<&str>, cancel: &AtomicBool) -> Result<()> {
        loop {
            if cancel.load(Ordering::Acquire) { return Err(Error::Cancelled); }
            let mut next = self.0.lock().map_err(|_| Error::Integrity)?;
            let now = Instant::now();
            let domain_key = domain.map(|d| format!("{lane}:{d}"));
            let due = next.get(lane).copied().unwrap_or(now).max(domain_key.as_ref().and_then(|k| next.get(k)).copied().unwrap_or(now));
            if due <= now {
                next.insert(lane.into(), now + Duration::from_millis(250));
                if let Some(key) = domain_key { next.insert(key, now + Duration::from_secs(1)); }
                return Ok(());
            }
            drop(next);
            std::thread::sleep(due.saturating_duration_since(now).min(Duration::from_millis(25)));
        }
    }
}

pub struct JsonReply { pub value: Value }

/// Opt-in live acceptance counter, compiled out of production. No key/body.
#[cfg(feature="m0-harness")]
pub fn charge_live(host:&str,operation:&str)->Result<()>{
    static GUARD:std::sync::Mutex<()>=std::sync::Mutex::new(());
    let Some(path)=std::env::var_os("LVJIN_REAL_BUDGET") else{return Ok(())};
    // The offline VM also uses a scripted generation fixture. Count only
    // actual MCP exchanges there, never confuse mock calls with live APIs.
    if std::env::var("LVJIN_REAL_MCP_ONLY").as_deref()==Ok("1")&&operation.starts_with('/') {return Ok(());}
    let _guard=GUARD.lock().map_err(|_|Error::Integrity)?;
    let path=std::path::PathBuf::from(path);
    let mut report:Value=if path.exists(){serde_json::from_slice(&std::fs::read(&path)?)?}else{serde_json::json!({"limit":30,"requests":[]})};
    // M3 and M4 each have a separately authorized 50-request allowance. Older stage
    // records retain their original 30-request hard ceiling.
    let ceiling=if matches!(report["stage"].as_str(),Some("M3"|"M4")){50}else{30};
    let limit=report["limit"].as_u64().unwrap_or(0).min(ceiling);
    let rows=report["requests"].as_array_mut().ok_or(Error::Contract)?;
    if rows.len() as u64>=limit{return Err(Error::Budget);}
    rows.push(serde_json::json!({"host":host,"operation":operation,"at":crate::vault::now_rfc3339()}));
    crate::durable::write(&path,&serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

/// The owned permit lives until the actual exchange ends, even on cancellation.
pub fn post(endpoint: Endpoint, operation: String, body: Value, key: Option<Zeroizing<String>>, options: HttpOptions,
    headers: Vec<(String, String)>, slots: &Slots, cancel: &AtomicBool) -> Result<JsonReply> {
    let permit = slots.acquire(cancel)?;
    let stop=Arc::new(AtomicBool::new(false));let stopped=stop.clone();
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let _permit: SlotGuard = permit;
        let bearer = key.as_ref().map(|k| Zeroizing::new(format!("Bearer {}", k.as_str())));
        let mut header_refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        if let Some(bearer) = &bearer { header_refs.push(("Authorization", bearer.as_str())); }
        let result = (|| {
            if stopped.load(Ordering::Acquire){return Err(Error::Cancelled);}
            let bytes = serde_json::to_vec(&body)?;
            let response = transport::post_json(&endpoint, &operation, &header_refs, &bytes, &options, false)?;
            if !(200..300).contains(&response.status) { return Err(Error::Remote(transport::status_code(response.status).into())); }
            let bytes = response.read_all(MAX_BODY)?;
            let value = serde_json::from_slice::<Value>(&bytes).map_err(|_|Error::Remote("malformed_response".into()))?;
            Ok(JsonReply { value })
        })();
        let _ = send.send(result);
    });
    loop {
        if cancel.load(Ordering::Acquire) { stop.store(true,Ordering::Release); return Err(Error::Cancelled); }
        match receive.recv_timeout(Duration::from_millis(25)) {
            Ok(value) => return value,
            Err(mpsc::RecvTimeoutError::Timeout) => {},
            Err(_) => return Err(Error::Integrity),
        }
    }
}
