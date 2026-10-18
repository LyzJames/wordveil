#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Desktop shell. The webview only renders and asks; every side effect goes
//! through the trusted core. Paths come from native dialogs, confirmations from
//! native dialogs built in Rust from the real prepared values, and the webview
//! can never choose a confirmed or protected action itself.
use lvjin_core::{
    Error,
    service::{Config, CoreService, Secret, TaskEvent},
    services::Layout,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use zeroize::Zeroizing;

const MILESTONE: &str = "M1";

struct Shell {
    core: Result<Arc<CoreService>, String>,
    production: bool,
}

type Reply = Result<Value, String>;

fn message(error: &Error) -> String {
    let text = match error {
        Error::Denied => "无权执行该操作，或口令/恢复码不正确。",
        Error::Locked => "私密空间已锁定，请先解锁。",
        Error::Cancelled => "操作已停止或空间已锁定。",
        Error::Conflict => "该操作与当前状态冲突（例如已有空间处于解锁状态）。",
        Error::Integrity => "完整性校验失败：安装资源、数据或备份与记录不一致。",
        Error::Contract => "请求不符合正式合同。",
        Error::Budget => "已达到本次任务的预算上限。",
        Error::NeedsInput(_) => "需要补充信息后才能继续。",
        Error::Worker(code) if code == "too_many_attempts" => "尝试次数过多，请稍等 30 秒后再试。",
        Error::Worker(_) => "本机处理失败。",
        _ => "操作失败。",
    };
    format!("{text}（{}）", error.code())
}

fn core<'a>(shell: &'a State<'_, Shell>) -> Result<&'a Arc<CoreService>, String> {
    shell.core.as_ref().map_err(Clone::clone)
}

fn secret(kind: &str, text: String) -> Result<Secret, String> {
    let text = Zeroizing::new(text);
    match kind {
        "passphrase" => Ok(Secret::Passphrase(text)),
        "recovery" => Ok(Secret::Recovery(text)),
        _ => Err("未知的解锁方式".into()),
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work).await.map_err(|_| "后台任务异常结束".to_string())?
}

fn confirm(app: &AppHandle, title: &str, text: &str, accept: &str) -> bool {
    app.dialog()
        .message(text)
        .title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(accept.into(), "取消".into()))
        .blocking_show()
}

fn picked(path: Option<tauri_plugin_dialog::FilePath>) -> Result<Option<PathBuf>, String> {
    path.map(|p| p.into_path().map_err(|_| "无法读取所选位置".to_string())).transpose()
}

// ------------------------------------------------------------------ status

#[tauri::command]
async fn app_status(shell: State<'_, Shell>) -> Reply {
    let production = shell.production;
    match shell.core.as_ref() {
        Err(error) => Ok(json!({"milestone": MILESTONE, "version": env!("CARGO_PKG_VERSION"), "installation_ok": false, "error": error})),
        Ok(core) => {
            let vaults = core.list_vaults().map_err(|e| message(&e))?;
            Ok(json!({"milestone": MILESTONE, "version": env!("CARGO_PKG_VERSION"), "installation_ok": true, "production": production, "unlocked": core.is_unlocked(),
                "session": core.session_handle(), "vaults": vaults, "generation": "not_configured", "network": "not_in_m1"}))
        }
    }
}

#[tauri::command]
async fn touch(shell: State<'_, Shell>) -> Reply {
    core(&shell)?.touch();
    Ok(Value::Null)
}

// ------------------------------------------------------------ vault lifecycle

#[tauri::command]
async fn vault_create(shell: State<'_, Shell>, name: String, passphrase: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        let (info, recovery) = core.create_vault(Zeroizing::new(passphrase), &name).map_err(|e| message(&e))?;
        Ok(json!({"info": info, "recovery_code": recovery.as_str()}))
    })
    .await
}

#[tauri::command]
async fn vault_unlock(shell: State<'_, Shell>, handle: String, kind: String, secret_text: String) -> Reply {
    let core = core(&shell)?.clone();
    let secret = secret(&kind, secret_text)?;
    blocking(move || Ok(json!(core.unlock(&handle, secret).map_err(|e| message(&e))?))).await
}

#[tauri::command]
async fn vault_lock(app: AppHandle, shell: State<'_, Shell>) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.lock().map_err(|e| message(&e))).await?;
    let _ = app.emit("lvjin://locked", json!({"reason": "user"}));
    Ok(Value::Null)
}

#[tauri::command]
async fn vault_restore(app: AppHandle, shell: State<'_, Shell>, kind: String, secret_text: String) -> Reply {
    let core = core(&shell)?.clone();
    let secret = secret(&kind, secret_text)?;
    blocking(move || {
        let Some(folder) = picked(app.dialog().file().set_title("选择加密备份文件夹（LvjinBackup-…）").blocking_pick_folder())? else { return Ok(Value::Null) };
        let handle = core.restore_backup(&folder, secret).map_err(|e| message(&e))?;
        Ok(json!({"handle": handle}))
    })
    .await
}

#[tauri::command]
async fn change_passphrase(shell: State<'_, Shell>, current: String, new: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.change_passphrase(Zeroizing::new(current), Zeroizing::new(new)).map(|_| Value::Null).map_err(|e| message(&e))).await
}

// --------------------------------------------------------------- skill runs

/// Skills the webview may start with an ordinary UI action. Commits, exports,
/// maintenance and privacy changes have their own trusted commands below.
const UI_SKILLS: &[&str] = &[
    "matter-manage", "knowledge-index", "knowledge-search", "evidence-rerank", "evidence-read", "graph-relations", "finance-calculate", "report-export",
    "diagnostics", "document-ocr", "lvjin-retrieval", "contract-review", "loan-analysis", "budget-analysis", "image-review", "audio-review", "video-review",
    "media-decode", "audio-transcribe", "query-plan", "answer-compose", "web-research",
];

fn ui_input(skill: &str, input: &Value) -> Result<(), String> {
    if !UI_SKILLS.contains(&skill) {
        return Err("该功能只能通过专门的确认流程启动".into());
    }
    let refused = (skill == "matter-manage" && input["operation"] == json!("commit_change")) || (skill == "report-export" && input["operation"] == json!("commit"));
    if refused { Err("提交与导出必须经过原生确认对话框".into()) } else { Ok(()) }
}

#[tauri::command]
async fn run_skill(shell: State<'_, Shell>, skill: String, input: Value) -> Reply {
    ui_input(&skill, &input)?;
    let core = core(&shell)?.clone();
    blocking(move || core.run_skill(&skill, input, "ui").map_err(|e| message(&e))).await
}

#[tauri::command]
async fn start_skill(shell: State<'_, Shell>, skill: String, input: Value) -> Reply {
    ui_input(&skill, &input)?;
    let core = core(&shell)?.clone();
    blocking(move || core.start_skill(&skill, input, "ui").map(Value::String).map_err(|e| message(&e))).await
}

#[tauri::command]
async fn tasks(shell: State<'_, Shell>) -> Reply {
    Ok(json!(core(&shell)?.tasks().map_err(|e| message(&e))?))
}

#[tauri::command]
async fn task_cancel(shell: State<'_, Shell>, task_id: String) -> Reply {
    core(&shell)?.cancel_task(&task_id).map(|_| Value::Null).map_err(|e| message(&e))
}

// ------------------------------------------------------------ core objects

#[tauri::command]
async fn create_object(shell: State<'_, Shell>, kind: String, value: Value) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        let list = |field: &str| -> Vec<String> { value[field].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default() };
        let id = match kind.as_str() {
            "question" => core.create_question(value["text"].as_str().unwrap_or("")),
            "scope" => core.create_scope(&list("matters"), &list("versions")),
            "range" => core.create_range(value["media"].as_str().unwrap_or(""), value["start_s"].as_f64().unwrap_or(0.0), value["end_s"].as_f64()),
            "filter" => core.create_filter(value["kind"].as_str().unwrap_or(""), &list("ids")),
            "change" => core.draft_change(value["matter"].as_str().unwrap_or(""), &value["change"]),
            "component" => core.component_ref(value["name"].as_str().unwrap_or("")),
            _ => return Err("未知的对象类型".into()),
        };
        id.map(Value::String).map_err(|e| message(&e))
    })
    .await
}

// ------------------------------------------- trusted actions (native dialogs)

const IMPORT_EXTENSIONS: &[&str] = &["pdf", "docx", "xlsx", "csv", "tsv", "txt", "md", "png", "jpg", "jpeg", "bmp", "tif", "tiff", "webp", "wav", "mp3", "m4a", "flac", "ogg", "mp4", "mov", "mkv", "avi"];

#[tauri::command]
async fn import_files(app: AppHandle, shell: State<'_, Shell>, matter: String, mode: String) -> Reply {
    if !matches!(mode.as_str(), "copy" | "new_version") {
        return Err("未知的导入方式".into());
    }
    let core = core(&shell)?.clone();
    blocking(move || {
        let files = app.dialog().file().set_title("选择要导入私密空间的资料").add_filter("资料与媒体", IMPORT_EXTENSIONS).add_filter("所有文件", &["*"]).blocking_pick_files();
        let Some(files) = files else { return Ok(json!([])) };
        let mut started = vec![];
        for file in files {
            let path = file.into_path().map_err(|_| "无法读取所选文件".to_string())?;
            let selection = core.register_selection("SelectionRef", "import", &path).map_err(|e| message(&e))?;
            let task = core
                .start_skill("document-import", json!({"selection_id": selection["selection_id"], "target_ref": matter, "import_mode": mode}), "ui")
                .map_err(|e| message(&e))?;
            started.push(json!({"task_id": task, "display_name": selection["display_name"], "selection_id": selection["selection_id"]}));
        }
        Ok(json!(started))
    })
    .await
}

#[tauri::command]
async fn resume_import(shell: State<'_, Shell>, selection: String, matter: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.start_skill("document-import", json!({"selection_id": selection, "target_ref": matter, "import_mode": "copy"}), "ui").map(Value::String).map_err(|e| message(&e))).await
}

fn describe(change: &Value) -> (String, String, String) {
    let impact = &change["impact"];
    let matter = change["matter_title"].as_str().unwrap_or("");
    let n = |field: &str| impact[field].as_i64().unwrap_or(0);
    match change["type"].as_str().unwrap_or("") {
        "delete_document_version" => {
            let doc = &change["document"];
            (
                "删除资料版本".into(),
                format!(
                    "将从事务“{matter}”删除《{}》第 {} 版。\n\n一并删除：{} 页、{} 个文字块、{} 个检索片段、{} 条事实（其中已确认 {} 条）、{} 条金额记录、{} 个音视频记录，以及引用它的检索快照和导出草稿。\n\n已有备份不受影响；普通删除不能保证擦除硬盘上的物理残留。",
                    doc["display_name"].as_str().unwrap_or("未命名"), doc["version_no"], n("pages"), n("blocks"), n("chunks"), n("facts"), n("confirmed_facts"), n("cashflows"), n("media")
                ),
                "确认删除".into(),
            )
        }
        "delete_matter" => ("删除事务".into(), format!("将删除事务“{matter}”及其 {} 个资料版本、全部事实、金额记录、关系与待办。此操作不可撤销。", n("document_versions")), "确认删除".into()),
        "create_matter" => ("新建事务".into(), format!("将新建事务“{}”。", change["title"].as_str().unwrap_or("")), "新建".into()),
        "confirm_facts" => ("确认事实".into(), format!("将把事务“{matter}”中的 {} 条候选标记为已确认，并记录到来源原文的依据关系。", change["count"]), "确认".into()),
        "reject_facts" => ("排除候选".into(), format!("将把 {} 条候选标记为已排除。", change["count"]), "排除".into()),
        "confirm_cashflows" => ("确认金额记录".into(), format!("将确认事务“{matter}”中的 {} 条金额记录；只有已确认记录参与计算。", change["count"]), "确认".into()),
        "reject_cashflows" => ("排除金额记录".into(), format!("将排除 {} 条金额记录。", change["count"]), "排除".into()),
        "import_cashflows_from_table" => ("按列生成金额记录".into(), format!("将按所选列生成 {} 条候选金额记录（{} 行无法识别，将跳过）。候选需逐条或批量确认后才参与计算。", n("rows") - n("rows_with_errors"), n("rows_with_errors")), "生成候选".into()),
        "add_relation" => ("写入关系".into(), format!("将在事务“{matter}”写入一条明确关系（{}）。", impact["relation_type"].as_str().unwrap_or("")), "写入".into()),
        "remove_relation" => ("移除关系".into(), "将移除所选关系。".into(), "移除".into()),
        other => ("保存变更".into(), format!("将在事务“{matter}”保存变更（{other}）。"), "保存".into()),
    }
}

#[tauri::command]
async fn commit_change(app: AppHandle, shell: State<'_, Shell>, matter: String, change_ref: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        let change = core.describe_change(&change_ref).map_err(|e| message(&e))?;
        if change["status"] != json!("prepared") || change["matter_id"] != json!(matter) {
            return Err("请先预览该变更".into());
        }
        let (title, text, accept) = describe(&change);
        if !confirm(&app, &title, &text, &accept) {
            return Ok(json!({"status": "declined"}));
        }
        core.run_skill("matter-manage", json!({"matter_id": matter, "operation": "commit_change", "change_ref": change_ref}), "confirm").map_err(|e| message(&e))
    })
    .await
}

#[tauri::command]
async fn export_commit(app: AppHandle, shell: State<'_, Shell>, draft: String, result_refs: Vec<String>, format: String, file_name: String) -> Reply {
    if !matches!(format.as_str(), "markdown" | "csv") {
        return Err("未知的导出格式".into());
    }
    let core = core(&shell)?.clone();
    blocking(move || {
        let (label, extension) = if format == "csv" { ("CSV 表格", "csv") } else { ("Markdown 文档", "md") };
        let name: String = file_name.chars().filter(|c| !r#"\/:*?"<>|"#.contains(*c)).take(80).collect();
        let target = app.dialog().file().set_title("选择导出位置").set_file_name(format!("{}.{extension}", if name.is_empty() { "律金智库导出".into() } else { name })).add_filter(label, &[extension]).blocking_save_file();
        let Some(target) = picked(target)? else { return Ok(json!({"status": "declined"})) };
        let selection = core.register_selection("OutputSelectionRef", "export", &target).map_err(|e| message(&e))?;
        core.run_skill("report-export", json!({"result_refs": result_refs, "format": format, "operation": "commit", "prepared_export_id": draft, "selection_id": selection["selection_id"]}), "confirm")
            .map_err(|e| message(&e))
    })
    .await
}

#[tauri::command]
async fn backup(app: AppHandle, shell: State<'_, Shell>, operation: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        let (title, purpose) = match operation.as_str() {
            "backup" => ("选择保存加密备份的位置（例如移动硬盘）", "backup_destination"),
            "verify" => ("选择要校验的加密备份文件夹", "backup_source"),
            _ => return Err("未知的备份操作".into()),
        };
        let Some(folder) = picked(app.dialog().file().set_title(title).blocking_pick_folder())? else { return Ok(json!({"status": "declined"})) };
        if operation == "backup" && !confirm(&app, "创建加密备份", "将在所选位置创建一份加密备份。恢复时需要当前口令或恢复码；生成接口密钥与外部连接不在备份内。", "创建备份") {
            return Ok(json!({"status": "declined"}));
        }
        let selection = core.register_selection("MaintenanceSelectionRef", purpose, &folder).map_err(|e| message(&e))?;
        core.run_skill("vault-maintenance", json!({"operation": operation, "selection_id": selection["selection_id"]}), "confirm").map_err(|e| message(&e))
    })
    .await
}

#[tauri::command]
async fn privacy(shell: State<'_, Shell>, operation: String, payload: Value) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        let request = core.privacy_request(&operation, &payload).map_err(|e| message(&e))?;
        core.run_skill("privacy-settings", json!({"operation": operation, "ui_request_id": request}), "protected").map_err(|e| message(&e))
    })
    .await
}

// -------------------------------------------------------------- read views

#[tauri::command]
async fn view(shell: State<'_, Shell>, kind: String, id: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        match kind.as_str() {
            "result" => core.result(&id),
            "page" => core.page_view(&id),
            "version" => core.version_pages(&id),
            "segment_audio" => core.segment_audio(&id),
            "interrupted" => core.interrupted_imports().map(|v| json!(v)),
            "render_page" => core.render_page(&id),
            "media" => core.media_view(&id),
            "skills" => core.skills().map(|v| json!(v)),
            "models" => core.models(),
            _ => return Err("未知的视图".into()),
        }
        .map_err(|e| message(&e))
    })
    .await
}

// ----------------------------------------------------------------- startup

fn install_root() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Production: installed tree beside the executable, data root from the
/// installer. Development builds (dev-worker) use the verified docs layout.
fn layout(root: &Path) -> Result<(Layout, PathBuf, bool), String> {
    #[cfg(feature = "dev-worker")]
    {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let docs = source.join("../../docs").canonicalize().map_err(|_| "开发布局缺少 docs".to_string())?;
        let app = source.join("..").canonicalize().map_err(|_| "开发布局缺少应用目录".to_string())?;
        let plain = |p: PathBuf| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\").to_string());
        let (docs, app) = (plain(docs), plain(app));
        // The e2e build may point at a fresh synthetic data root; other builds cannot.
        let data = match std::env::var_os("LVJIN_E2E_DATA_ROOT").map(PathBuf::from).filter(|p| cfg!(feature = "e2e") && p.is_absolute()) {
            Some(path) => path,
            None => docs.join("runtime/m1-dev"),
        };
        let webview = docs.join("build/webview2-fixed/Microsoft.WebView2.FixedVersionRuntime.155.0.4283.60.x64");
        let _ = root;
        return Layout::development(&docs, &app, &data).map(|l| (l, webview, false)).map_err(|e| message(&e));
    }
    #[cfg(not(feature = "dev-worker"))]
    {
        let data = std::fs::read_to_string(root.join("data-root.txt"))
            .ok()
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| p.is_absolute() && !p.starts_with(root))
            .ok_or_else(|| "安装缺少有效的数据目录设置（data-root.txt），请用完整安装介质修复".to_string())?;
        Layout::production(root, &data).map(|l| (l, root.join("runtimes/webview2"), true)).map_err(|_| "安装资源或九模型清单校验失败，请用同版本完整离线介质修复".to_string())
    }
}

fn main() {
    let Some(root) = install_root() else { return };
    let (core, webview, production) = match layout(&root) {
        Ok((layout, webview, production)) => {
            let data = layout.data_root.clone();
            if std::fs::create_dir_all(data.join("webview2")).is_err() {
                return;
            }
            // Before any thread or WebView2 exists: bundled fixed runtime only,
            // no runtime download, no background networking or updates.
            let debugging = if cfg!(feature = "e2e") { " --remote-debugging-port=9333" } else { "" };
            let arguments = format!("--disable-background-networking --disable-component-update --disable-sync --disable-features=msSmartScreenProtection{debugging}");
            unsafe {
                std::env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER", &webview);
                std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", data.join("webview2"));
                std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", arguments);
            }
            let handle: Arc<OnceLock<AppHandle>> = Arc::new(OnceLock::new());
            let sink_handle = handle.clone();
            let sink = Box::new(move |event: TaskEvent| {
                if let Some(app) = sink_handle.get() {
                    let _ = app.emit("lvjin://task", &event);
                }
            });
            let core = CoreService::new(layout, Config::default(), sink).map_err(|e| message(&e));
            (core.map(|c| (c, handle)), webview, production)
        }
        Err(error) => (Err(error), root.join("runtimes/webview2"), true),
    };
    if !webview.join("msedgewebview2.exe").is_file() {
        return;
    }
    let (core, handle) = match core {
        Ok((core, handle)) => (Ok(core), Some(handle)),
        Err(error) => (Err(error), None),
    };
    let protocol_core = core.as_ref().ok().cloned();
    let lock_core = core.as_ref().ok().cloned();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .register_asynchronous_uri_scheme_protocol("lvjin", move |ctx, request, responder| {
            // Capabilities guard IPC, not custom schemes: serve only the main
            // window, and only unexpired random tokens issued by the core.
            let main_window = ctx.webview_label() == "main";
            let core = protocol_core.clone();
            std::thread::spawn(move || {
                let token = request.uri().path().trim_start_matches('/').to_owned();
                let served = core.as_ref().filter(|_| main_window && token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())).and_then(|c| c.serve_token(&token).ok());
                let response = match served {
                    Some((bytes, mime)) => tauri::http::Response::builder().header("Content-Type", mime).header("Cache-Control", "no-store").header("X-Content-Type-Options", "nosniff").body(bytes),
                    None => tauri::http::Response::builder().status(404).header("Cache-Control", "no-store").body(Vec::new()),
                };
                if let Ok(response) = response {
                    responder.respond(response);
                }
            });
        })
        .manage(Shell { core, production })
        .setup(move |app| {
            if let Some(handle) = handle {
                let _ = handle.set(app.handle().clone());
            }
            if let Some(core) = lock_core {
                let app = app.handle().clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(Duration::from_secs(20));
                    if let Ok(true) = core.auto_lock_tick() {
                        let _ = app.emit("lvjin://locked", json!({"reason": "idle"}));
                    }
                });
            }
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title(&format!("律金智库 · {MILESTONE} 内部版"));
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(shell) = window.try_state::<Shell>() {
                    if let Ok(core) = shell.core.as_ref() {
                        let _ = core.lock();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_status, touch, vault_create, vault_unlock, vault_lock, vault_restore, change_passphrase, run_skill, start_skill, tasks, task_cancel,
            create_object, import_files, resume_import, commit_change, export_commit, backup, privacy, view
        ])
        .run(tauri::generate_context!())
        .expect("desktop runtime failed");
}
