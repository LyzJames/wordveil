//! Core-controlled AppContainer launcher. No capabilities, exact inherited pipes,
//! one-process kill-on-close Job, bounded IPC, sanitized environment and RAII teardown.
use crate::{Error, Result};
use std::{collections::BTreeMap, ffi::c_void, fs::File, io::{BufReader, Read, Write}, os::windows::{ffi::OsStrExt, io::{FromRawHandle, RawHandle}}, path::{Path,PathBuf}, sync::{atomic::{AtomicBool,Ordering},mpsc}, thread, time::{Duration,Instant}};
use windows_sys::Win32::{Foundation::{HANDLE,CloseHandle,SetHandleInformation,HANDLE_FLAG_INHERIT,WAIT_OBJECT_0},Security::SECURITY_ATTRIBUTES,System::{Pipes::CreatePipe,Threading::{TerminateProcess,WaitForSingleObject},JobObjects::TerminateJobObject}};

unsafe extern "C" {
    fn lvjin_profile_create(name:*const u16,sid:*mut *mut c_void)->u32;
    fn lvjin_profile_free(sid:*mut c_void);
    fn lvjin_profile_delete(name:*const u16)->u32;
    fn lvjin_grant_path(path:*const u16,sid:*mut c_void,writable:i32)->u32;
    fn lvjin_revoke_path(path:*const u16,sid:*mut c_void)->u32;
    fn lvjin_capability_sid(name:*const u16,sid:*mut *mut c_void)->u32;
    fn lvjin_grant_runtime(path:*const u16,capability:*mut c_void)->u32;
    fn lvjin_cleanup_profiles(prefix:*const u16,removed:*mut u32)->u32;
    fn lvjin_spawn_container(executable:*const u16,command:*mut u16,working:*const u16,environment:*mut u16,sid:*mut c_void,runtime:*mut c_void,input:HANDLE,output:HANDLE,error:HANDLE,memory_limit:usize,process:*mut HANDLE,job:*mut HANDLE,pid:*mut u32)->u32;
}

fn wide(value:impl AsRef<std::ffi::OsStr>)->Vec<u16> { value.as_ref().encode_wide().chain(Some(0)).collect() }
fn process_path(path:&Path)->PathBuf {
    // Preserve canonical identity for policy checks, use Win32 drive spelling at
    // the process boundary. Some Windows loaders reject verbatim cwd spellings.
    let text=path.as_os_str().to_string_lossy();
    if let Some(rest)=text.strip_prefix("\\\\?\\") { if rest.as_bytes().get(1)==Some(&b':') { return PathBuf::from(rest); } }
    path.to_path_buf()
}
fn check(code:u32)->Result<()> { if code==0 { Ok(()) } else { Err(std::io::Error::from_raw_os_error(code as i32).into()) } }
fn quote(value:&str)->String {
    let mut output=String::from("\""); let mut slashes=0;
    for c in value.chars() {
        if c=='\\' { slashes+=1; continue; }
        if c=='\"' { output.push_str(&"\\".repeat(slashes*2+1)); output.push(c); }
        else { output.push_str(&"\\".repeat(slashes)); output.push(c); }
        slashes=0;
    }
    output.push_str(&"\\".repeat(slashes*2)); output.push('"'); output
}

/// Granting and revoking read a directory's DACL, change it and write it back
/// over the whole tree. A worker evicted on the pool thread revokes its SID
/// from the same shared directories another thread is granting to a new
/// child; unserialized, the later writer drops the other's entry and that
/// child cannot start or read its files.
static ACL_CHANGES:std::sync::Mutex<()>=std::sync::Mutex::new(());

/// The application's read-only runtime capability (custom capability SID,
/// derived from its name). Directories of the worker bundle, model artefacts
/// and FFmpeg grant it once, inheritably, instead of one AppContainer SID per
/// process start; private and sensitive paths keep per-process grants.
const RUNTIME_CAPABILITY:&str="lvjinWorkerRuntime";
struct Sid(*mut c_void);
unsafe impl Send for Sid {}
unsafe impl Sync for Sid {}
static CAPABILITY:std::sync::OnceLock<std::result::Result<Sid,i32>>=std::sync::OnceLock::new();
static RUNTIME_GRANTED:std::sync::Mutex<Vec<PathBuf>>=std::sync::Mutex::new(Vec::new());

fn runtime_capability()->Result<*mut c_void> {
    let found=CAPABILITY.get_or_init(|| {
        let mut sid=std::ptr::null_mut();
        match unsafe { lvjin_capability_sid(wide(RUNTIME_CAPABILITY).as_ptr(),&mut sid) } { 0=>Ok(Sid(sid)), code=>Err(code as i32) }
    });
    match found { Ok(sid)=>Ok(sid.0), Err(code)=>Err(std::io::Error::from_raw_os_error(*code).into()) }
}

/// Grant the runtime capability on a shared read-only directory, once per
/// process (and only rewritten on disk when the ACE is missing).
fn grant_runtime(path:&Path)->Result<()> {
    let path=path.canonicalize()?;
    let capability=runtime_capability()?;
    let mut granted=RUNTIME_GRANTED.lock().unwrap_or_else(|poisoned|poisoned.into_inner());
    if granted.contains(&path) { return Ok(()); }
    let _serialized=ACL_CHANGES.lock().unwrap_or_else(|poisoned|poisoned.into_inner());
    check(unsafe { lvjin_grant_runtime(wide(&path).as_ptr(),capability) })?;
    granted.push(path);
    Ok(())
}

/// Remove worker profiles a crash or a killed process left behind (startup).
/// Skipped while another instance of the application runs: its workers' live
/// profiles have the same prefix.
pub fn cleanup_stale_profiles()->u32 {
    use windows_sys::Win32::{Foundation::{ERROR_ALREADY_EXISTS,GetLastError},System::Threading::CreateMutexW};
    static INSTANCE:std::sync::OnceLock<usize>=std::sync::OnceLock::new();
    let mut first=false;
    INSTANCE.get_or_init(|| {
        let handle=unsafe { CreateMutexW(std::ptr::null(),0,wide("Local\\Lvjin.CoreInstance").as_ptr()) };
        first=!handle.is_null() && unsafe { GetLastError() }!=ERROR_ALREADY_EXISTS;
        handle as usize
    });
    if !first { return 0; }
    let mut removed=0u32;
    let _serialized=ACL_CHANGES.lock().unwrap_or_else(|poisoned|poisoned.into_inner());
    unsafe { lvjin_cleanup_profiles(wide("lvjin.worker.").as_ptr(),&mut removed); }
    removed
}

struct Profile { name:Vec<u16>,sid:*mut c_void,granted:Vec<PathBuf> }
unsafe impl Send for Profile {}
impl Profile {
    fn new()->Result<Self> {
        let name=wide(format!("Lvjin.Worker.{}",uuid::Uuid::new_v4().simple()));
        let mut sid=std::ptr::null_mut();
        check(unsafe { lvjin_profile_create(name.as_ptr(),&mut sid) })?;
        Ok(Self { name,sid,granted:vec![] })
    }
    fn grant(&mut self,path:&Path,writable:bool)->Result<()> {
        let path=path.canonicalize()?;
        self.granted.push(path.clone());
        let _serialized=ACL_CHANGES.lock().unwrap_or_else(|poisoned|poisoned.into_inner());
        check(unsafe { lvjin_grant_path(wide(&path).as_ptr(),self.sid,i32::from(writable)) })?;
        Ok(())
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        let _serialized=ACL_CHANGES.lock().unwrap_or_else(|poisoned|poisoned.into_inner());
        unsafe {
            for path in self.granted.iter().rev() { lvjin_revoke_path(wide(path).as_ptr(),self.sid); }
            lvjin_profile_free(self.sid); lvjin_profile_delete(self.name.as_ptr());
        }
    }
}

fn pipe(parent_reads:bool)->Result<(File,File)> {
    let mut read=std::ptr::null_mut(); let mut write=std::ptr::null_mut();
    let attributes=SECURITY_ATTRIBUTES { nLength:std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,lpSecurityDescriptor:std::ptr::null_mut(),bInheritHandle:1 };
    if unsafe { CreatePipe(&mut read,&mut write,&attributes,0) }==0 { return Err(std::io::Error::last_os_error().into()); }
    let parent=if parent_reads { read } else { write };
    if unsafe { SetHandleInformation(parent,HANDLE_FLAG_INHERIT,0) }==0 {
        unsafe { CloseHandle(read); CloseHandle(write); } return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { (File::from_raw_handle(read as RawHandle),File::from_raw_handle(write as RawHandle)) })
}

/// Constructed only by the trusted installation/worker coordinator. Business JSON
/// does not deserialize this policy or choose an executable, path, argument or limit.
pub struct SpawnPolicy {
    pub executable:PathBuf,pub arguments:Vec<String>,
    /// Shared read-only runtime (worker bundle, model artefacts, tools):
    /// granted once to the runtime capability every worker holds.
    pub runtime:Vec<PathBuf>,
    /// Private read-only inputs (for example a decrypted media file): granted
    /// to this process's AppContainer only and revoked when it ends.
    pub read_only:Vec<PathBuf>,
    pub scratch:PathBuf,pub memory_limit:usize,
}

pub struct IsolatedProcess {
    process:HANDLE,job:HANDLE,pub pid:u32,input:Option<mpsc::SyncSender<(Vec<u8>,mpsc::Sender<Result<()>>)>>,
    receiver:Option<mpsc::Receiver<Result<Vec<u8>>>>,reader:Option<thread::JoinHandle<()>>,
    writer:Option<thread::JoinHandle<()>>,
    _profile:Profile,stopped:bool,
    error_reader:Option<thread::JoinHandle<()>>,
    #[cfg(feature="m0-harness")] diagnostics:std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}
unsafe impl Send for IsolatedProcess {}

impl IsolatedProcess {
    pub fn spawn(policy:SpawnPolicy)->Result<Self> {
        use std::os::windows::io::AsRawHandle;
        let executable=process_path(&policy.executable.canonicalize()?);
        if !executable.is_file() || policy.memory_limit<256*1024*1024 { return Err(Error::Contract); }
        let scratch=process_path(&policy.scratch.canonicalize()?);
        if !scratch.is_dir() { return Err(Error::Contract); }
        let mut profile=Profile::new()?;
        // Windows rewrites TEMP/TMP for an AppContainer to
        // LOCALAPPDATA/Packages/<profile>/AC/Temp. Materialize it beneath the
        // private workdir before applying the inherited SID and Low label.
        let profile_name=String::from_utf16(&profile.name[..profile.name.len()-1]).map_err(|_|Error::Integrity)?;
        std::fs::create_dir_all(scratch.join("Packages").join(profile_name).join("AC/Temp"))?;
        #[cfg(feature="m0-harness")] eprintln!("isolation diagnostic: profile created");
        for path in &policy.runtime { grant_runtime(path)?; }
        let capability=if policy.runtime.is_empty() { std::ptr::null_mut() } else { runtime_capability()? };
        for path in &policy.read_only { profile.grant(path,false)?; }
        #[cfg(feature="m0-harness")] eprintln!("isolation diagnostic: resources granted");
        profile.grant(&scratch,true)?;
        #[cfg(feature="m0-harness")] eprintln!("isolation diagnostic: scratch granted");
        let (child_input,parent_input)=pipe(false)?;
        let (parent_output,child_output)=pipe(true)?;
        let (parent_error,child_error)=pipe(true)?;
        let executable_text=executable.to_str().ok_or(Error::Contract)?;
        let command=std::iter::once(executable_text.to_owned()).chain(policy.arguments).map(|item|quote(&item)).collect::<Vec<_>>().join(" ");
        let mut command=wide(command);
        let root=std::env::var_os("SystemRoot").ok_or(Error::Integrity)?;
        let mut environment:BTreeMap<String,String>=BTreeMap::new();
        let scratch_text=scratch.to_str().ok_or(Error::Contract)?.to_owned();
        let root_text=root.to_str().ok_or(Error::Contract)?.to_owned();
        for key in ["TEMP","TMP","HOME","USERPROFILE","LOCALAPPDATA","APPDATA"] { environment.insert(key.into(),scratch_text.clone()); }
        // os.makedirs checks a directory's parent even when exist_ok=True. Give
        // vendor caches a child of the granted workdir, never an ungranted parent.
        for key in ["HF_HOME","TORCH_HOME","PADDLE_HOME","PADDLE_PDX_CACHE_HOME","XDG_CACHE_HOME","NUMBA_CACHE_DIR","MPLCONFIGDIR","CUDA_CACHE_PATH"] { environment.insert(key.into(),scratch.join("cache").to_str().ok_or(Error::Contract)?.into()); }
        // Windows redirects the AppContainer TEMP into Packages/<profile>/AC/Temp.
        // Inductor imports create a default cache there even for eager inference;
        // the added profile path can exceed MAX_PATH in an ordinary nested vault.
        // Keep it in this worker's granted scratch, without a system-policy change.
        environment.insert("TORCHINDUCTOR_CACHE_DIR".into(),scratch.join("cache/torchinductor").to_str().ok_or(Error::Contract)?.into());
        for (key,value) in [("SystemRoot",root_text.clone()),("WINDIR",root_text.clone()),("PATH",format!("{};{}\\System32",executable.parent().ok_or(Error::Contract)?.display(),root_text)),("PYTHONNOUSERSITE","1".into()),("PYTHONUTF8","1".into()),("PYTHONDONTWRITEBYTECODE","1".into()),("HF_HUB_OFFLINE","1".into()),("TRANSFORMERS_OFFLINE","1".into()),("HF_HUB_DISABLE_TELEMETRY","1".into()),("DO_NOT_TRACK","1".into()),("PADDLE_PDX_DISABLE_MODEL_SOURCE_CHECK","True".into()),("CUDA_VISIBLE_DEVICES","".into()),("TOKENIZERS_PARALLELISM","false".into()),("OMP_NUM_THREADS","4".into()),("MKL_NUM_THREADS","4".into())] { environment.insert(key.into(),value); }
        let mut block:Vec<u16>=environment.iter().flat_map(|(key,value)|wide(format!("{key}={value}"))).collect(); block.push(0);
        let mut process=std::ptr::null_mut(); let mut job=std::ptr::null_mut(); let mut pid=0;
        check(unsafe { lvjin_spawn_container(wide(&executable).as_ptr(),command.as_mut_ptr(),wide(&scratch).as_ptr(),block.as_mut_ptr(),profile.sid,capability,child_input.as_raw_handle() as HANDLE,child_output.as_raw_handle() as HANDLE,child_error.as_raw_handle() as HANDLE,policy.memory_limit,&mut process,&mut job,&mut pid) })?;
        #[cfg(feature="m0-harness")] eprintln!("isolation diagnostic: process launched");
        drop(child_input); drop(child_output); drop(child_error);
        // A hostile child can stop reading stdin. The coordinator must keep
        // polling cancellation while a dedicated thread performs WriteFile.
        let (input,input_receiver)=mpsc::sync_channel::<(Vec<u8>,mpsc::Sender<Result<()>>)>(1);
        let writer=thread::spawn(move || {
            let mut stream=parent_input;
            while let Ok((bytes,complete))=input_receiver.recv() {
                let outcome=(||->Result<()> {stream.write_all(&bytes)?;stream.write_all(b"\n")?;stream.flush()?;Ok(())})();
                let failed=outcome.is_err();let _=complete.send(outcome);
                if failed {break;}
            }
        });
        // Errors are intentionally discarded, never copied into ordinary product logs.
        #[cfg(feature="m0-harness")] let diagnostics=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        #[cfg(feature="m0-harness")] let reader_diagnostics=diagnostics.clone();
        let error_reader=thread::spawn(move || {
            #[cfg(feature="m0-harness")] {
                let mut data=Vec::new(); let _=parent_error.take(64*1024).read_to_end(&mut data);
                if let Ok(mut target)=reader_diagnostics.lock() { *target=data; }
            }
            #[cfg(not(feature="m0-harness"))] { let _=std::io::copy(&mut {parent_error},&mut std::io::sink()); }
        });
        let (sender,receiver)=mpsc::sync_channel(2);
        let reader=thread::spawn(move || {
            let mut stream=BufReader::new(parent_output);
            loop {
                let mut line=Vec::new();
                let result=(||->Result<()> {
                    // read_exact avoids an unbounded read_until allocation on hostile IPC.
                    loop { let mut byte=[0u8;1]; stream.read_exact(&mut byte)?; if byte[0]==b'\n' { return Ok(()); } if line.len()>=24*1024*1024 { return Err(Error::Contract); } line.push(byte[0]); }
                })();
                let failed=result.is_err();
                if sender.send(result.map(|_|line)).is_err() || failed { break; }
            }
        });
        Ok(Self { process,job,pid,input:Some(input),receiver:Some(receiver),reader:Some(reader),writer:Some(writer),error_reader:Some(error_reader),_profile:profile,stopped:false,#[cfg(feature="m0-harness")] diagnostics })
    }

    #[cfg(feature="m0-harness")]
    pub fn synthetic_diagnostics(&mut self)->String {
        let mut code=0; unsafe { windows_sys::Win32::System::Threading::GetExitCodeProcess(self.process,&mut code); }
        if code!=259 { if let Some(reader)=self.error_reader.take() { let _=reader.join(); } }
        format!("exit={code:#x}; stderr={}",String::from_utf8_lossy(&self.diagnostics.lock().unwrap()))
    }

    pub fn send(&mut self,value:&serde_json::Value,cancellation:&AtomicBool,deadline:Instant)->Result<()> {
        if cancellation.load(Ordering::Acquire) || Instant::now()>=deadline {self.terminate()?;return Err(Error::Cancelled);}
        let bytes=serde_json::to_vec(value)?;
        if bytes.len()>18*1024*1024 { return Err(Error::Contract); }
        let (complete,completion)=mpsc::channel();
        self.input.as_ref().ok_or(Error::Cancelled)?.try_send((bytes,complete)).map_err(|_|Error::Integrity)?;
        loop {
            if cancellation.load(Ordering::Acquire) || Instant::now()>=deadline {self.terminate()?;return Err(Error::Cancelled);}
            match completion.recv_timeout(Duration::from_millis(25)) {
                Ok(outcome)=>return outcome,
                Err(mpsc::RecvTimeoutError::Timeout)=>{},
                Err(_)=>return Err(Error::Integrity),
            }
        }
    }

    pub fn receive(&mut self,cancellation:&AtomicBool,deadline:Instant)->Result<serde_json::Value> {
        Ok(serde_json::from_slice(&self.receive_line(cancellation,deadline)?)?)
    }

    /// The next stdout message if one arrives within `wait` (None on timeout).
    /// An exited child surfaces as the reader's end-of-file error.
    pub fn poll(&mut self,cancellation:&AtomicBool,wait:Duration)->Result<Option<serde_json::Value>> {
        if cancellation.load(Ordering::Acquire) { self.terminate()?; return Err(Error::Cancelled); }
        match self.receiver.as_ref().ok_or(Error::Cancelled)?.recv_timeout(wait) {
            Ok(line)=>Ok(Some(serde_json::from_slice(&line?)?)),
            Err(mpsc::RecvTimeoutError::Timeout)=>Ok(None),
            Err(_)=>Err(Error::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))),
        }
    }

    #[cfg(feature="m0-harness")]
    pub fn receive_synthetic_probe_line(&mut self,cancellation:&AtomicBool,deadline:Instant)->Result<Vec<u8>> {
        self.receive_line(cancellation,deadline)
    }

    fn receive_line(&mut self,cancellation:&AtomicBool,deadline:Instant)->Result<Vec<u8>> {
        loop {
            if cancellation.load(Ordering::Acquire) || Instant::now()>=deadline { self.terminate()?; return Err(Error::Cancelled); }
            match self.receiver.as_ref().ok_or(Error::Cancelled)?.recv_timeout(Duration::from_millis(25)) {
                Ok(line)=>return line,
                Err(mpsc::RecvTimeoutError::Timeout)=>{},
                Err(_)=>return Err(Error::Integrity),
            }
        }
    }

    /// Read stdout lines until the tool closes it (bounded), for one-shot tools
    /// such as ffprobe/ffmpeg whose output is not line-delimited JSON.
    pub fn read_until_eof(&mut self,cancellation:&AtomicBool,deadline:Instant,max_bytes:usize)->Result<Vec<u8>> {
        let mut output=Vec::new();
        loop {
            match self.receive_line(cancellation,deadline) {
                Ok(line)=>{
                    if output.len()+line.len()+1>max_bytes { self.terminate()?; return Err(Error::Contract); }
                    output.extend_from_slice(&line);output.push(b'\n');
                }
                Err(Error::Io(error)) if error.kind()==std::io::ErrorKind::UnexpectedEof=>return Ok(output),
                Err(error)=>return Err(error),
            }
        }
    }

    /// Wait for the process to exit within the deadline; returns its exit code.
    pub fn wait_exit(&mut self,cancellation:&AtomicBool,deadline:Instant)->Result<u32> {
        loop {
            if cancellation.load(Ordering::Acquire) || Instant::now()>=deadline { self.terminate()?; return Err(Error::Cancelled); }
            if unsafe { WaitForSingleObject(self.process,25) }==WAIT_OBJECT_0 {
                let mut code=0u32;
                if unsafe { windows_sys::Win32::System::Threading::GetExitCodeProcess(self.process,&mut code) }==0 { return Err(std::io::Error::last_os_error().into()); }
                return Ok(code);
            }
        }
    }

    pub fn memory(&self)->Result<crate::model_pool::ModelMemory> {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo,PROCESS_MEMORY_COUNTERS_EX};
        let mut counters:PROCESS_MEMORY_COUNTERS_EX=unsafe {std::mem::zeroed()};
        counters.cb=std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if unsafe {K32GetProcessMemoryInfo(self.process,(&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),counters.cb)}==0 {return Err(std::io::Error::last_os_error().into());}
        Ok(crate::model_pool::ModelMemory{working_set:counters.WorkingSetSize,peak_working_set:counters.PeakWorkingSetSize,private_bytes:counters.PrivateUsage,peak_private_commit:counters.PeakPagefileUsage})
    }

    pub fn terminate(&mut self)->Result<()> {
        if self.stopped { return Ok(()); }
        self.input.take();
        unsafe { TerminateJobObject(self.job,1); TerminateProcess(self.process,1); }
        if unsafe { WaitForSingleObject(self.process,10_000) }!=WAIT_OBJECT_0 { return Err(Error::Integrity); }
        self.stopped=true;
        // Killing the only reader releases a blocked WriteFile. Closing the
        // bounded output receiver also releases a producer blocked by flooding.
        self.receiver.take();
        if let Some(writer)=self.writer.take() {writer.join().map_err(|_|Error::Integrity)?;}
        if let Some(reader)=self.reader.take() {reader.join().map_err(|_|Error::Integrity)?;}
        Ok(())
    }
}
impl Drop for IsolatedProcess {
    fn drop(&mut self) {
        let _=self.terminate();
        unsafe { CloseHandle(self.job); CloseHandle(self.process); }
        if let Some(reader)=self.error_reader.take() { let _=reader.join(); }
        self.reader.take();
    }
}
