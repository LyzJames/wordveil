//! The core owns workers, not roles. One resident owner per artifact across keys.
use std::{collections::BTreeMap, sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, atomic::{AtomicBool, Ordering}}, thread, time::{Duration, Instant}};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelKey {
    pub model: String, pub revision: String, pub artifact: String,
    pub engine: String, pub device: String, pub profile: String,
}
impl ModelKey {
    fn owner(&self) -> (String,String,String) { (self.model.clone(),self.revision.clone(),self.artifact.clone()) }
    fn large(&self) -> bool { !matches!(self.model.as_str(),"doc-ori"|"textline-ori") }
}

/// Large-model residency (active plus idle). Default: one large owner. With at
/// least 16 GiB installed memory the embedding/reranker pair may stay resident
/// together (user decision 2026-10-17); heavy stages remain serialized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResidencyPolicy { pub max_large: usize, pub coresident: Vec<String> }
impl Default for ResidencyPolicy {
    fn default() -> Self { Self { max_large: 1, coresident: vec![] } }
}
impl ResidencyPolicy {
    pub const TIER_BYTES: u64 = 16 * 1024 * 1024 * 1024;
    pub fn for_installed_memory(bytes: Option<u64>) -> Self {
        match bytes {
            Some(value) if value >= Self::TIER_BYTES => Self { max_large: 2, coresident: vec!["embedding".into(), "reranker".into()] },
            _ => Self::default(),
        }
    }
    fn may_add(&self, existing: &[String], model: &str) -> bool {
        existing.is_empty()
            || (existing.len() < self.max_large
                && self.coresident.iter().any(|m| m == model)
                && existing.iter().all(|e| self.coresident.iter().any(|m| m == e)))
    }
    fn max_idle(&self) -> usize { if self.max_large >= 2 { 2 } else { 1 } }
}

/// Installed (not OS-visible) physical memory, matching the user-facing size.
#[cfg(windows)]
pub fn installed_memory_bytes() -> Option<u64> {
    let mut kilobytes = 0u64;
    let ok = unsafe { windows_sys::Win32::System::SystemInformation::GetPhysicallyInstalledSystemMemory(&mut kilobytes) };
    if ok != 0 && kilobytes > 0 { Some(kilobytes * 1024) } else { None }
}

#[derive(Clone)]
pub struct AllowedArtifact {
    pub model: String, pub revision: String, pub artifact: String,
    pub verified_keys: Vec<ModelKey>,
}

/// Real implementations must terminate and wait for the isolated worker in shutdown.
/// A Lease release alone never asserts that tensor memory has been released.
pub trait ManagedModel: Send {
    fn predict(&mut self, input: &Value, cancellation: &AtomicBool) -> Result<Value>;
    fn shutdown(&mut self) -> Result<()>;
    fn process_id(&self)->Option<u32> { None }
    fn memory(&self)->Option<ModelMemory> {None}
}

#[derive(Clone,Debug,Serialize)]
pub struct ModelMemory {pub working_set:usize,pub peak_working_set:usize,pub private_bytes:usize,pub peak_private_commit:usize}
type Factory = dyn Fn(&ModelKey,&AtomicBool,Instant) -> Result<Box<dyn ManagedModel>> + Send + Sync;
type Owner = (String,String,String);
struct Resident { key:ModelKey, leases:usize, idle_since:Option<Instant>, model:Arc<Mutex<Box<dyn ManagedModel>>>, failed:bool }
struct State { residents:BTreeMap<Owner,Resident>, faulted:bool }
struct Inner { state:Mutex<State>, heavy:Mutex<()>, changed:Condvar, factory:Box<Factory>, allowed:Vec<AllowedArtifact>, idle_ttl:Duration, residency:ResidencyPolicy }

#[derive(Clone)]
pub struct ModelPool { inner:Arc<Inner> }
pub struct ModelLease { inner:Arc<Inner>, key:ModelKey }

impl ModelPool {
    /// Test-only policy pools never create production owners.
    #[cfg(feature="m0-harness")]
    pub fn new(allowed:Vec<AllowedArtifact>, factory:Box<Factory>) -> Self {
        Self::new_inner(allowed,factory,ResidencyPolicy::default())
    }
    #[cfg(feature="m0-harness")]
    pub fn new_with(allowed:Vec<AllowedArtifact>, factory:Box<Factory>, residency:ResidencyPolicy) -> Self {
        Self::new_inner(allowed,factory,residency)
    }
    /// All production installations in this core process share one coordinator.
    /// A different manifest cannot create a parallel owner with another engine.
    pub(crate) fn production(allowed:Vec<AllowedArtifact>,identity:String,residency:ResidencyPolicy,factory:Box<Factory>)->Result<Self> {
        static GLOBAL:std::sync::OnceLock<Mutex<Option<(String,Arc<Inner>)>>>=std::sync::OnceLock::new();
        let mut global=GLOBAL.get_or_init(||Mutex::new(None)).lock().map_err(|_|Error::Integrity)?;
        if let Some((existing,inner))=global.as_ref() {
            return if existing==&identity {Ok(Self{inner:inner.clone()})} else {Err(Error::Conflict)};
        }
        let pool=Self::new_inner(allowed,factory,residency);
        *global=Some((identity,pool.inner.clone()));Ok(pool)
    }
    fn new_inner(allowed:Vec<AllowedArtifact>, factory:Box<Factory>, residency:ResidencyPolicy) -> Self {
        let inner=Arc::new(Inner { state:Mutex::new(State { residents:BTreeMap::new(),faulted:false }),heavy:Mutex::new(()),changed:Condvar::new(),factory,allowed,idle_ttl:Duration::from_secs(60),residency });
        let weak=Arc::downgrade(&inner);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(1));
            let Some(inner)=weak.upgrade() else { break; };
            if let Ok(mut state)=inner.state.try_lock() {
                let stale:Vec<_>=state.residents.iter().filter(|(_,r)|r.leases==0 && r.idle_since.is_some_and(|idle|idle.elapsed()>=inner.idle_ttl)).map(|(id,_)|id.clone()).collect();
                for owner in stale { if evict(&mut state,&owner).is_err() { break; } }
                inner.changed.notify_all();
            }
        });
        Self { inner }
    }

    pub fn acquire(&self, key:&ModelKey, cancellation:&AtomicBool, deadline:Instant) -> Result<ModelLease> {
        if !self.inner.allowed.iter().any(|artifact| artifact.model==key.model && artifact.revision==key.revision && artifact.artifact==key.artifact && artifact.verified_keys.contains(key)) { return Err(Error::Denied); }
        let mut state=lock_cancellable(&self.inner.state,cancellation,deadline)?;
        loop {
            if cancellation.load(Ordering::Acquire) || Instant::now()>=deadline { return Err(Error::Cancelled); }
            if state.faulted { return Err(Error::Integrity); }
            let stale:Vec<_>=state.residents.iter().filter(|(_,resident)| resident.leases==0 && resident.idle_since.is_some_and(|idle|idle.elapsed()>=self.inner.idle_ttl)).map(|(owner,_)|owner.clone()).collect();
            for owner in stale { evict(&mut state,&owner)?; }
            let owner=key.owner();
            if let Some(resident)=state.residents.get_mut(&owner) {
                if resident.failed {
                    if resident.leases==0 { evict(&mut state,&owner)?; continue; }
                    return Err(Error::Integrity);
                }
                if resident.key==*key {
                    resident.leases+=1;
                    resident.idle_since=None;
                    return Ok(ModelLease { inner:self.inner.clone(),key:key.clone() });
                }
                if resident.leases==0 {
                    evict(&mut state,&owner)?;
                    continue;
                }
            } else {
                let large:Vec<(Owner,usize,String)>=state.residents.iter().filter(|(_,resident)|resident.key.large()).map(|(owner,resident)|(owner.clone(),resident.leases,resident.key.model.clone())).collect();
                let names:Vec<String>=large.iter().map(|(_,_,model)|model.clone()).collect();
                let coresides=!key.large() || self.inner.residency.may_add(&names,&key.model);
                if !coresides && large.iter().any(|(_,leases,_)|*leases>0) {
                    // Another active owner must release before a different large load.
                } else {
                    if !coresides {
                        for (owner,_,_) in &large { evict(&mut state,owner)?; }
                    }
                    // Holding this core gate makes loading singleflight and serializes
                    // heavy inference/load. No second serving owner can escape the gate.
                    let _heavy=lock_cancellable(&self.inner.heavy,cancellation,deadline)?;
                    let mut model=(self.inner.factory)(key,cancellation,deadline)?;
                    if cancellation.load(Ordering::Acquire) || Instant::now()>=deadline {
                        model.shutdown()?;
                        return Err(Error::Cancelled);
                    }
                    state.residents.insert(owner,Resident { key:key.clone(),leases:1,idle_since:None,model:Arc::new(Mutex::new(model)),failed:false });
                    return Ok(ModelLease { inner:self.inner.clone(),key:key.clone() });
                }
            }
            let remaining=deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(50));
            state=self.inner.changed.wait_timeout(state,remaining).map_err(|_|Error::Integrity)?.0;
        }
    }

    pub fn evict_idle(&self) -> Result<()> {
        let mut state=self.inner.state.lock().map_err(|_|Error::Integrity)?;
        let idle:Vec<_>=state.residents.iter().filter(|(_,resident)|resident.leases==0).map(|(owner,_)|owner.clone()).collect();
        for owner in idle { evict(&mut state,&owner)?; }
        Ok(())
    }

    pub fn resident_count(&self) -> Result<usize> { Ok(self.inner.state.lock().map_err(|_|Error::Integrity)?.residents.len()) }

    #[cfg(feature="m0-harness")]
    pub fn resident_processes(&self)->Result<Vec<u32>> {
        let state=self.inner.state.lock().map_err(|_|Error::Integrity)?;
        let mut output=vec![];
        for resident in state.residents.values() { if let Some(pid)=resident.model.lock().map_err(|_|Error::Integrity)?.process_id() { output.push(pid); } }
        Ok(output)
    }
}

fn evict(state:&mut State,owner:&Owner) -> Result<()> {
    if let Some(resident)=state.residents.get_mut(owner) {
        if resident.leases!=0 { return Err(Error::Conflict); }
        if let Err(error)=resident.model.lock().map_err(|_|Error::Integrity)?.shutdown() { state.faulted=true; return Err(error); }
    }
    state.residents.remove(owner);
    Ok(())
}

fn lock_cancellable<'a,T>(mutex:&'a Mutex<T>,cancellation:&AtomicBool,deadline:Instant)->Result<MutexGuard<'a,T>> {
    loop {
        if cancellation.load(Ordering::Acquire) || Instant::now()>=deadline { return Err(Error::Cancelled); }
        match mutex.try_lock() {
            Ok(guard)=>return Ok(guard),
            Err(TryLockError::Poisoned(_))=>return Err(Error::Integrity),
            Err(TryLockError::WouldBlock)=>thread::sleep(Duration::from_millis(10)),
        }
    }
}

impl ModelLease {
    pub fn memory(&self)->Result<Option<ModelMemory>> {
        let state=self.inner.state.lock().map_err(|_|Error::Integrity)?;
        let resident=state.residents.get(&self.key.owner()).ok_or(Error::Integrity)?;
        if resident.key!=self.key || resident.failed {return Err(Error::Integrity);}
        Ok(resident.model.lock().map_err(|_|Error::Integrity)?.memory())
    }
    pub fn predict(&self,input:&Value,cancellation:&AtomicBool) -> Result<Value> {
        if cancellation.load(Ordering::Acquire) { return Err(Error::Cancelled); }
        let deadline=Instant::now()+Duration::from_secs(120);
        let state=lock_cancellable(&self.inner.state,cancellation,deadline)?;
        if state.faulted { return Err(Error::Integrity); }
        let resident=state.residents.get(&self.key.owner()).ok_or(Error::Integrity)?;
        if resident.key!=self.key || resident.failed { return Err(Error::Integrity); }
        let model=resident.model.clone();
        drop(state);
        let _heavy=lock_cancellable(&self.inner.heavy,cancellation,deadline)?;
        let mut worker=lock_cancellable(&model,cancellation,deadline)?;
        let result=worker.predict(input,cancellation);
        if result.is_err() {
            let stopped=worker.shutdown();
            drop(worker);
            drop(_heavy);
            let mut state=self.inner.state.lock().map_err(|_|Error::Integrity)?;
            if let Some(resident)=state.residents.get_mut(&self.key.owner()) { resident.failed=true; }
            if stopped.is_err() { state.faulted=true; }
        }
        result
    }
}

impl Drop for ModelLease {
    fn drop(&mut self) {
        if let Ok(mut state)=self.inner.state.lock() {
            if let Some(resident)=state.residents.get_mut(&self.key.owner()) {
                resident.leases=resident.leases.saturating_sub(1);
                if resident.leases==0 { resident.idle_since=Some(Instant::now()); }
            }
            let mut idle:Vec<_>=state.residents.iter().filter_map(|(owner,resident)|resident.idle_since.map(|time|(owner.clone(),time))).collect();
            idle.sort_by_key(|(_,time)|*time);
            let excess=idle.len().saturating_sub(self.inner.residency.max_idle());
            for (owner,_) in idle.into_iter().take(excess) { if evict(&mut state,&owner).is_err() { break; } }
            self.inner.changed.notify_all();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(state)=self.state.get_mut() {
            for resident in state.residents.values_mut() { if let Ok(mut model)=resident.model.lock() { let _=model.shutdown(); } }
        }
    }
}
