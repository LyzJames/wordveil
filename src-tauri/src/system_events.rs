//! Windows session lock and sleep (design 12.2): an unattended session must
//! not keep the private vault open. A message-only window receives the
//! session notifications (WTSRegisterSessionNotification delivers them to
//! the registered window, they are not broadcasts) and a suspend/resume
//! callback reports sleep without any window. The shell decides what to do.
use std::{
    ffi::c_void,
    sync::{Arc, OnceLock},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        Power::{DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, RegisterSuspendResumeNotification},
        RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification},
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, DEVICE_NOTIFY_CALLBACK, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, PBT_APMSUSPEND, RegisterClassExW, TranslateMessage,
        WM_WTSSESSION_CHANGE, WNDCLASSEXW, WTS_SESSION_LOCK,
    },
};

/// Called with "session_lock" or "suspend". Must return quickly; for a
/// suspend Windows waits for the callback before the machine sleeps.
pub type Handler = Arc<dyn Fn(&'static str) + Send + Sync>;

static HANDLER: OnceLock<Handler> = OnceLock::new();

/// Start watching (once per process).
pub fn watch(handler: Handler) {
    if HANDLER.set(handler).is_err() {
        return;
    }
    // Sleep: the callback form needs no window; the parameters must outlive
    // the registration, which lasts for the whole process.
    let parameters: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS { Callback: Some(on_power), Context: std::ptr::null_mut() }));
    unsafe {
        RegisterSuspendResumeNotification(parameters as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as HANDLE, DEVICE_NOTIFY_CALLBACK);
    }
    let _ = std::thread::Builder::new().name("lvjin-session-watch".into()).spawn(|| unsafe { session_window() });
}

unsafe extern "system" fn on_power(_context: *const c_void, kind: u32, _setting: *const c_void) -> u32 {
    if kind == PBT_APMSUSPEND {
        if let Some(handler) = HANDLER.get() {
            handler("suspend");
        }
    }
    0
}

unsafe fn session_window() {
    let class: Vec<u16> = "LvjinSessionWatch\0".encode_utf16().collect();
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let mut description: WNDCLASSEXW = unsafe { std::mem::zeroed() };
    description.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    description.lpfnWndProc = Some(window_proc);
    description.hInstance = instance;
    description.lpszClassName = class.as_ptr();
    if unsafe { RegisterClassExW(&description) } == 0 {
        return;
    }
    let window = unsafe { CreateWindowExW(0, class.as_ptr(), std::ptr::null(), 0, 0, 0, 0, 0, HWND_MESSAGE, std::ptr::null_mut(), instance, std::ptr::null()) };
    if window.is_null() {
        return;
    }
    // Remote Desktop Services may not be ready right after logon.
    for _ in 0..30 {
        if unsafe { WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION) } != 0 {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let mut message: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_WTSSESSION_CHANGE && wparam as u32 == WTS_SESSION_LOCK {
        if let Some(handler) = HANDLER.get() {
            handler("session_lock");
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}
