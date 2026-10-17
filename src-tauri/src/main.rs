#![cfg_attr(not(debug_assertions),windows_subsystem="windows")]
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
struct InstallationStatus { stage:&'static str,version:&'static str,contracts:usize,roles:usize,resources_verified:bool,models:Vec<String>,cpu_backend:&'static str }

fn installation_root()->Result<PathBuf,String> {
    std::env::current_exe().ok().and_then(|path|path.parent().map(|p|p.to_owned())).ok_or_else(||"无法定位安装目录".into())
}

#[tauri::command]
fn installation_status()->Result<InstallationStatus,String> {
    let root=installation_root()?;
    let registry=lvjin_core::runtime::Registry::load(&root.join("resources"),false).map_err(|_|"安装资源校验失败，请检查完整安装介质".to_owned())?;
    let installation=lvjin_core::installation::Installation::load(&root).map_err(|_|"九模型安装清单校验失败，请检查完整安装介质".to_owned())?;
    Ok(InstallationStatus {stage:"M0",version:env!("CARGO_PKG_VERSION"),contracts:registry.skills.len(),roles:registry.roles.len(),resources_verified:true,
        models:installation.artifacts.into_iter().map(|v|v.key.model).collect(),cpu_backend:"CPU"})
}

fn main() {
    let Ok(root)=installation_root() else { return; };
    let fixed=root.join("runtimes/webview2");
    if !fixed.join("msedgewebview2.exe").is_file() { return; }
    // The installer writes an absolute data-root setting. Development previews
    // use docs/runtime; portable fallback stays beside the installed application.
    let data=std::fs::read_to_string(root.join("data-root.txt")).ok().map(|s|PathBuf::from(s.trim()))
        .filter(|p|p.is_absolute() && !p.starts_with(&root)).unwrap_or_else(||root.parent().unwrap_or(&root).join("LvjinData-M0"));
    if std::fs::create_dir_all(data.join("webview2")).is_err(){return;}
    // The process has not started threads or created WebView2 yet. The bundled
    // runtime is selected explicitly; runtime installation/download is disabled.
    unsafe {
        std::env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",fixed);
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER",data.join("webview2"));
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS","--disable-background-networking --disable-component-update --disable-sync");
    }
    tauri::Builder::default().invoke_handler(tauri::generate_handler![installation_status])
        .run(tauri::generate_context!()).expect("desktop runtime failed");
}
