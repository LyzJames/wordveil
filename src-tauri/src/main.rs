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

const MILESTONE: &str = "M2";

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
        Error::NeedsInput("api_key") => "需要填写该连接的 API Key。",
        Error::NeedsInput("connection") => "还没有可用的生成连接，请先在“生成接口”中添加并保存。",
        Error::NeedsInput("base_url") => "服务地址格式不正确（应以 https:// 开头）。",
        Error::NeedsInput("base_url_https") => "非本机服务必须使用 https:// 地址。",
        Error::NeedsInput("base_url_operation_path") => "服务地址只填到根地址，不要包含 /chat/completions 等接口路径。",
        Error::NeedsInput("model_id") => "请填写模型 ID。",
        Error::NeedsInput("name") => "请填写显示名称（60 字以内）。",
        Error::NeedsInput("protocol") => "所选服务不支持该协议。",
        Error::NeedsInput("timeout_s") => "超时需在 5–600 秒之间。",
        Error::NeedsInput("max_output_tokens") => "单次输出上限需在 256–65536 之间。",
        Error::NeedsInput(_) => "需要补充信息后才能继续。",
        Error::Remote(_) => "生成接口请求失败。",
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

// ------------------------------------------------- generation connections

#[tauri::command]
async fn connections(shell: State<'_, Shell>) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.connections().map(Value::from).map_err(|e| message(&e))).await
}

/// Secure input channel for the API key: it goes straight to the core, which
/// seals it with DPAPI; the protected settings Skill only sees a draft ref.
#[tauri::command]
async fn connection_save(shell: State<'_, Shell>, draft: Value, api_key: Option<String>) -> Reply {
    let core = core(&shell)?.clone();
    let key = api_key.map(Zeroizing::new);
    blocking(move || {
        let draft = core.connection_draft(&draft, key).map_err(|e| message(&e))?;
        core.run_skill("connection-settings", json!({"operation": "save", "connection_draft_id": draft}), "protected").map_err(|e| message(&e))
    })
    .await
}

#[tauri::command]
async fn connection_probe(app: AppHandle, shell: State<'_, Shell>, connection: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        let info = core.describe_connection(&connection).map_err(|e| message(&e))?;
        let text = format!(
            "将向 {}（模型 {}）发送约 7 条合成测试请求，检测文本、流式、工具调用、结构化输出、图片输入和音频输入能力。\n\n测试内容是固定的合成样本，不包含你的任何资料。{}",
            info["host"].as_str().unwrap_or(""),
            info["model_id"].as_str().unwrap_or(""),
            if info["loopback"] == json!(true) { "这是本机地址的服务；它是否继续转发到其他地方无法由本应用核验。" } else { "可能产生少量费用。" }
        );
        if !confirm(&app, "检测生成接口能力", &text, "开始检测") {
            return Err("已取消".into());
        }
        let draft = core.connection_reference(&connection).map_err(|e| message(&e))?;
        core.start_skill("connection-settings", json!({"operation": "probe", "connection_draft_id": draft}), "protected").map(Value::String).map_err(|e| message(&e))
    })
    .await
}

#[tauri::command]
async fn connection_revoke(app: AppHandle, shell: State<'_, Shell>, connection: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        let info = core.describe_connection(&connection).map_err(|e| message(&e))?;
        let text = format!("将删除本机保存的 {} 的 API Key，并停止使用这个连接。\n\n已经发送给服务方的内容无法从远端撤回。", info["host"].as_str().unwrap_or(""));
        if !confirm(&app, "删除生成连接", &text, "删除") {
            return Err("已取消".into());
        }
        let draft = core.connection_reference(&connection).map_err(|e| message(&e))?;
        core.run_skill("connection-settings", json!({"operation": "revoke", "connection_draft_id": draft}), "protected").map_err(|e| message(&e))
    })
    .await
}

#[tauri::command]
async fn connection_default(shell: State<'_, Shell>, connection: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.set_default_connection(&connection).map(|_| Value::Null).map_err(|e| message(&e))).await
}

/// Local retrieval for the preview; registers an unapproved, single-use grant.
#[tauri::command]
async fn answer_preview(shell: State<'_, Shell>, question: String, matters: Vec<String>, connection: Option<String>, deep: Option<bool>) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.answer_preview_with(&question, &matters, connection.as_deref(), deep.unwrap_or(false)).map_err(|e| message(&e))).await
}

/// Written analysis preview: local evidence, fixed roles, request limits.
#[tauri::command]
async fn analysis_preview(shell: State<'_, Shell>, kind: String, request: Value) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.analysis_preview(&kind, &request).map_err(|e| message(&e))).await
}

/// Media review preview: the images to upload (prepared now) or the bounded
/// frame/audio policy of a video or audio range.
#[tauri::command]
async fn media_preview(shell: State<'_, Shell>, kind: String, request: Value) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.media_preview(&kind, &request).map_err(|e| message(&e))).await
}

/// The native dialog is built here from the grant's real parameters; only a
/// confirmed dialog approves the grant and starts the task.
#[tauri::command]
async fn answer_start(app: AppHandle, shell: State<'_, Shell>, grant: String, excluded: Vec<String>, excluded_media: Option<Vec<String>>) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        let excluded_media = excluded_media.unwrap_or_default();
        let preview = core.describe_grant(&grant).map_err(|e| message(&e))?;
        let text = egress_text(&preview, excluded.len(), &excluded_media);
        if !confirm(&app, "向生成接口发送资料", &text, "发送并生成") {
            let _ = core.answer_cancel_preview(&grant);
            return Err("已取消，未发送任何内容".into());
        }
        core.grant_start(&grant, &excluded, &excluded_media).map(Value::String).map_err(|e| message(&e))
    })
    .await
}

/// The confirmation text, from the grant's stored real parameters only.
fn egress_text(preview: &Value, excluded: usize, excluded_media: &[String]) -> String {
    let purpose = match preview["purpose"].as_str().unwrap_or("") {
        "deep_answer" => "深入分析这一个问题（任务规划者安排至多 3 个专业角色，检查者复核，不包含联网搜索）",
        "contract_review" => "合同书面分析（法律适用、金融口径、证据检查、结果综合）",
        "loan_analysis" => "借款书面分析（金融口径、法律适用、证据检查、结果综合）",
        "budget_analysis" => "预算书面分析（金融口径、证据检查、结果综合）",
        "image_review" => "图片分析（直接读取所列图片，并与本机识别文字核对；不一致时针对有分歧的图片再读一次）",
        "audio_review" => "录音分析（本机转写；接口通过音频自检时直接听取所选时段）",
        "video_review" => "视频分析（本机抽帧识别与转写；接口通过自检时直接读取抽帧画面与音轨）",
        _ => "生成这一次回答（不包含联网搜索）",
    };
    let offered = preview["excerpts"].as_array().map(|e| e.len()).unwrap_or(0);
    let media: Vec<&Value> = preview["native_media"].as_array().map(|m| m.iter().filter(|x| !excluded_media.iter().any(|e| x["media_ref"] == e.as_str())).collect()).unwrap_or_default();
    let mut lines = vec![
        format!("接收方：{}（{}）", preview["connection"]["name"].as_str().unwrap_or(""), preview["connection"]["host"].as_str().unwrap_or("")),
        format!("模型：{}", preview["connection"]["model_id"].as_str().unwrap_or("")),
        format!("用途：{purpose}"),
        format!("将发送：你的问题，以及所选资料中的原文摘录（预览中 {} 段；任务中可能在同一范围内替换，全程最多 {} 段）", offered.saturating_sub(excluded), preview["max_items"].as_i64().unwrap_or(0)),
    ];
    if !media.is_empty() {
        let bytes: u64 = media.iter().filter_map(|m| m["bytes"].as_u64()).sum();
        lines.push(format!("将上传原始图片：{} 张（JPEG，长边不超过 2048 像素，共约 {} KB）；与本机识别不一致时，有分歧的图片会再上传一次", media.len(), bytes.div_ceil(1024)));
    }
    if let Some(policy) = preview["media_policy"].as_object() {
        if policy["frames"].as_u64().unwrap_or(0) > 0 {
            lines.push(format!("将上传：所选时段内均匀抽取的画面，最多 {} 张（JPEG）；不上传音频时，与本机识别不一致的画面可能再上传一次", policy["frames"]));
        }
        if policy["audio_seconds"].as_f64().unwrap_or(0.0) > 0.0 {
            lines.push(format!("将上传：所选时段的原始音频，最长 {:.0} 秒", policy["audio_seconds"].as_f64().unwrap_or(0.0)));
        }
    }
    lines.push(format!("请求次数：最多 {} 次", preview["max_requests"].as_i64().unwrap_or(0)));
    lines.push("有效期：仅本次任务".into());
    format!("{}\n\n已发送的内容无法从服务方撤回。", lines.join("\n"))
}

#[tauri::command]
async fn answer_discard(shell: State<'_, Shell>, grant: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.answer_cancel_preview(&grant).map(|_| Value::Null).map_err(|e| message(&e))).await
}

// ------------------------------------------------- interrupted flow runs

#[tauri::command]
async fn flows_interrupted(shell: State<'_, Shell>) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.interrupted_flows().map(Value::from).map_err(|e| message(&e))).await
}

/// Continuing sends further requests under the original grant, so it needs
/// its own native confirmation built from the run's real state.
#[tauri::command]
async fn flow_continue(app: AppHandle, shell: State<'_, Shell>, run: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || {
        let info = core.describe_continue(&run).map_err(|e| message(&e))?;
        let unknown = info["unknown_requests"].as_i64().unwrap_or(0);
        let (used, max) = (info["used_requests"].as_i64().unwrap_or(0), info["max_requests"].as_i64().unwrap_or(0));
        let text = format!(
            "问题：{}\n接收方：{}（{}）\n模型：{}\n已用请求：{} / {} 次\n{}{}\n继续后从中断处接着执行，已完成的步骤不会重复。已发送的内容无法从服务方撤回。",
            info["question"].as_str().unwrap_or("（无）"),
            info["connection"]["name"].as_str().unwrap_or(""),
            info["connection"]["host"].as_str().unwrap_or(""),
            info["connection"]["model_id"].as_str().unwrap_or(""),
            used,
            max,
            if unknown > 0 { format!("有 {unknown} 次请求在中断时已发出但没有收到回应，结果未知，已计入额度；继续时该步骤会在剩余额度内重新请求。\n") } else { String::new() },
            if max - used <= 2 { "剩余额度不多：可能不够再做检查与修订，回答会按“部分完成”交付并标出未核实的句子。\n" } else { "" },
        );
        if !confirm(&app, "继续中断的任务", &text, "继续") {
            return Err("已取消，未发送任何内容".into());
        }
        core.flow_continue(&run).map(Value::String).map_err(|e| message(&e))
    })
    .await
}

#[tauri::command]
async fn flow_discard(shell: State<'_, Shell>, run: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.flow_discard(&run).map(|_| Value::Null).map_err(|e| message(&e))).await
}

#[tauri::command]
async fn generation_requests(shell: State<'_, Shell>) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.generation_requests(200).map(Value::from).map_err(|e| message(&e))).await
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
                "session": core.session_handle(), "vaults": vaults}))
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

#[tauri::command]
async fn feedback_remove(shell: State<'_, Shell>, id: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.remove_feedback(&id).map(|_| Value::Null).map_err(|e| message(&e))).await
}

#[tauri::command]
async fn task_retry(shell: State<'_, Shell>, task_id: String) -> Reply {
    let core = core(&shell)?.clone();
    blocking(move || core.retry_task(&task_id).map(Value::String).map_err(|e| message(&e))).await
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
            "feedback" => core.create_feedback(value["result"].as_str().unwrap_or(""), value["claim_id"].as_str(), value["issue"].as_str().unwrap_or(""), value["note"].as_str().unwrap_or("")),
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
                    "将从事务“{matter}”删除《{}》第 {} 版。\n\n一并删除：{} 页、{} 个文字块、{} 个检索片段、{} 条事实（其中已确认 {} 条）、{} 条金额记录、{} 个音视频记录，以及引用它的检索快照、回答与分析、导出草稿、为生成接口准备的媒体副本和外发预览中的摘录。\n\n已有备份不受影响；普通删除不能保证擦除硬盘上的物理残留。",
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
            "page_thumb" => core.page_thumb(&id),
            "media" => core.media_view(&id),
            "skills" => core.skills().map(|v| json!(v)),
            "models" => core.models(),
            "lock_policy" => core.lock_policy(),
            "feedback" => core.feedback(&id),
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
        let webview = docs.join("build/webview2-fixed/Microsoft.WebView2.FixedVersionRuntime.155.0.4283.62.x64");
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
    let watch_core = core.as_ref().ok().cloned();
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
            // Windows lock screen or sleep: lock the vault (setting, on by default).
            if let Some(core) = watch_core {
                let app = app.handle().clone();
                lvjin_core::system_events::watch(Arc::new(move |event: &'static str| {
                    if !core.lock_on_system_lock() {
                        return;
                    }
                    let (core, app) = (core.clone(), app.clone());
                    let (sent, done) = std::sync::mpsc::channel::<()>();
                    std::thread::spawn(move || {
                        if core.lock().is_ok() {
                            let _ = app.emit("lvjin://locked", json!({"reason": event}));
                        }
                        let _ = sent.send(());
                    });
                    // Windows waits for this callback before sleeping: give the
                    // lock a moment to drop the session and stop the workers.
                    if event == "suspend" {
                        let _ = done.recv_timeout(Duration::from_secs(2));
                    }
                }));
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
            app_status, touch, vault_create, vault_unlock, vault_lock, vault_restore, change_passphrase, run_skill, start_skill, tasks, task_cancel, task_retry,
            create_object, feedback_remove, import_files, resume_import, commit_change, export_commit, backup, privacy, view, connections, connection_save,
            connection_probe, connection_revoke, connection_default, answer_preview, answer_start, answer_discard, generation_requests,
            flows_interrupted, flow_continue, flow_discard, analysis_preview, media_preview
        ])
        .run(tauri::generate_context!())
        .expect("desktop runtime failed");
}
