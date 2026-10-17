//! report-export (preview, then commit to a real destination selection) and
//! diagnostics (redacted status only). Design 12.6/12.7, OWASP CSV injection.
use super::*;
use crate::{runtime::RESULT_KINDS, store::sha256_hex, vault::now_rfc3339};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::io::Write;

/// OWASP CSV injection: quote every field, double quotes, and neutralize cells
/// starting with formula/control characters (incl. full-width variants) with a
/// leading apostrophe. Pure decimal numbers cannot form a formula and stay as-is.
pub fn csv_field(raw: &str) -> String {
    let numeric = {
        let body = raw.strip_prefix('-').unwrap_or(raw);
        !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit() || b == b'.') && body.matches('.').count() <= 1 && !body.starts_with('.')
    };
    let risky = raw.starts_with(['=', '+', '-', '@', '\t', '\r', '\n', '＝', '＋', '－', '＠']);
    let value = if risky && !numeric { format!("'{raw}") } else { raw.to_owned() };
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub fn csv_line(fields: &[String]) -> String {
    fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",") + "\r\n"
}

/// Markdown escaping so document text cannot create links, images or HTML.
pub fn md(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '!' | '<' | '>' | '#' | '|' | '{' | '}') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn s(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn render(kind: &str, payload: &Value, format: &str, out: &mut String, csv_rows: &mut Vec<Vec<String>>) {
    match kind {
        "CalculationRef" => {
            out.push_str(&format!("## 计算结果：{}\n\n", md(&s(&payload["calculation_type"]))));
            out.push_str(&format!("- 计算引擎：{}\n- 口径：{}\n- 计算时间：{}\n\n", md(&s(&payload["engine"])), md(&payload["assumptions_used"].to_string()), md(&s(&payload["computed_at"]))));
            out.push_str("```json\n");
            out.push_str(&serde_json::to_string_pretty(&payload["result"]).unwrap_or_default().replace("```", "'''"));
            out.push_str("\n```\n\n");
            flatten("计算", "", &payload["result"], csv_rows);
        }
        "MatterSnapshotRef" => {
            out.push_str(&format!("## 事务：{}\n\n", md(&s(&payload["matter"]["title"]))));
            out.push_str("### 事实\n\n| 类型 | 名称 | 内容 | 状态 | 来源 |\n|---|---|---|---|---|\n");
            for fact in payload["facts"].as_array().cloned().unwrap_or_default() {
                let content = fact["value"].as_object().map(|v| v.values().map(s).collect::<Vec<_>>().join(" ")).unwrap_or_default();
                out.push_str(&format!("| {} | {} | {} | {} | {} |\n", md(&s(&fact["fact_type"])), md(&s(&fact["label"])), md(&content), md(&s(&fact["status"])), md(&s(&fact["origin"]))));
                csv_rows.push(vec!["事实".into(), s(&fact["fact_type"]), s(&fact["label"]), content, s(&fact["status"]), s(&fact["origin"])]);
            }
            out.push_str("\n### 金额记录\n\n| 日期 | 方向 | 金额 | 币种 | 类型 | 对方 | 状态 |\n|---|---|---:|---|---|---|---|\n");
            for row in payload["cashflows"].as_array().cloned().unwrap_or_default() {
                out.push_str(&format!("| {} | {} | {} | {} | {} | {} | {} |\n", md(&s(&row["date"])), if row["direction"] == "in" { "收入" } else { "支出" }, md(&s(&row["amount"])), md(&s(&row["currency"])), md(&s(&row["kind"])), md(&s(&row["counterparty"])), md(&s(&row["status"]))));
                csv_rows.push(vec!["金额记录".into(), s(&row["date"]), s(&row["direction"]), s(&row["amount"]), s(&row["currency"]), s(&row["kind"]), s(&row["counterparty"]), s(&row["status"])]);
            }
            out.push_str("\n### 时间线\n\n");
            for item in payload["timeline"].as_array().cloned().unwrap_or_default() {
                out.push_str(&format!("- {} {}（{}）\n", md(&s(&item["date"])), md(&s(&item["label"])), md(&s(&item["status"]))));
            }
            out.push('\n');
        }
        "EvidenceReadRef" | "RetrievalResultRef" | "EvidenceBundleRef" => {
            out.push_str("## 原文依据\n\n");
            let items = payload["items"].as_array().or_else(|| payload["read"]["items"].as_array()).cloned().unwrap_or_default();
            for (index, item) in items.iter().enumerate() {
                let pages: Vec<String> = item["locators"].as_array().cloned().unwrap_or_default().iter().filter_map(|l| l["page_no"].as_i64().map(|p| p.to_string())).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
                out.push_str(&format!("{}. 《{}》第 {} 版{}\n\n> {}\n\n", index + 1, md(&s(&item["document"]["display_name"])), s(&item["document"]["version_no"]),
                    if pages.is_empty() { String::new() } else { format!("，第 {} 页", pages.join("、")) }, md(&s(&item["exact_text"])).replace('\n', "\n> ")));
                csv_rows.push(vec!["原文依据".into(), s(&item["document"]["display_name"]), pages.join(";"), s(&item["exact_text"])]);
            }
        }
        _ => {
            out.push_str(&format!("## {}\n\n```json\n{}\n```\n\n", md(kind), serde_json::to_string_pretty(payload).unwrap_or_default().replace("```", "'''")));
        }
    }
    let _ = format;
}

fn flatten(prefix: &str, path: &str, value: &Value, rows: &mut Vec<Vec<String>>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                flatten(prefix, &if path.is_empty() { key.clone() } else { format!("{path}.{key}") }, child, rows);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                flatten(prefix, &format!("{path}[{index}]"), child, rows);
            }
        }
        other => rows.push(vec![prefix.into(), path.into(), s(other)]),
    }
}

pub fn export(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let operation = text_of(input, "operation")?;
    let format = text_of(input, "format")?;
    let refs = refs_of(input, "result_refs")?;
    if refs.is_empty() || refs.len() > 50 {
        return Err(Error::Contract);
    }
    if operation == "prepare" {
        let mut markdown = format!("# 律金智库导出\n\n- 生成时间：{}\n- 说明：本文件由本机确定性流程整理；“候选”内容未经确认，不构成法律或财务结论。\n\n", now_rfc3339());
        let mut rows: Vec<Vec<String>> = vec![];
        {
            let vault = ctx.lock()?;
            for id in &refs {
                let (_, kind, payload) = vault.get_result(id)?;
                if !RESULT_KINDS.contains(&kind.as_str()) {
                    return Err(Error::Denied);
                }
                render(&kind, &payload, &format, &mut markdown, &mut rows);
            }
        }
        let content = if format == "csv" {
            let mut text = String::from("\u{feff}");
            text.push_str(&csv_line(&["类别".into(), "字段1".into(), "字段2".into(), "字段3".into(), "字段4".into(), "字段5".into(), "字段6".into(), "字段7".into()]));
            for row in &rows {
                text.push_str(&csv_line(row));
            }
            text
        } else {
            markdown
        };
        let vault = ctx.lock()?;
        let preview: String = content.chars().take(20000).collect();
        let draft = vault.put_result(invocation.skill(), invocation.id(), "ExportDraftRef", &json!({"format": format, "bytes": content.len(), "sha256": sha256_hex(content.as_bytes()), "result_refs": refs, "preview": preview}))?;
        let blob = vault.put_blob(&draft, "export_draft", content.as_bytes())?;
        vault.db().execute(
            "INSERT INTO exports(id,format,result_refs,content_blob,content_sha256,status,created_at) VALUES(?,?,?,?,?,'prepared',?)",
            params![draft, format, serde_json::to_string(&refs)?, blob, sha256_hex(content.as_bytes()), now_rfc3339()],
        )?;
        return Ok(completed(Some(draft)));
    }
    if operation != "commit" {
        return Err(Error::Contract);
    }
    let draft = input["prepared_export_id"].as_str().ok_or(Error::NeedsInput("prepared_export_id"))?.to_owned();
    let selection = input["selection_id"].as_str().ok_or(Error::NeedsInput("selection_id"))?.to_owned();
    ctx.authorize(invocation, "file.export")?;
    let vault = ctx.lock()?;
    if vault.runtime.root_action(invocation.root_id())? != "confirm" {
        return Err(Error::Denied);
    }
    let key = format!("export:{draft}:{selection}");
    if let Some(outcome) = vault.db().query_row("SELECT outcome FROM commit_log WHERE idempotency_key=?", [&key], |r| r.get::<_, String>(0)).optional()? {
        let outcome: Value = serde_json::from_str(&outcome)?;
        let mut parts = completed(outcome["result_ref"].as_str().map(str::to_owned));
        warn(&mut parts, "already_exported", "该导出已完成，未重复写入。");
        return Ok(parts);
    }
    let (stored_format, stored_refs, blob, digest, status): (String, String, String, String, String) = vault
        .db()
        .query_row("SELECT e.format,e.result_refs,e.content_blob,e.content_sha256,e.status FROM exports e JOIN objects o ON o.id=e.id WHERE e.id=? AND o.active=1", [&draft], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .optional()?
        .ok_or(Error::Denied)?;
    let stored_refs: Vec<String> = serde_json::from_str(&stored_refs)?;
    if stored_format != format || stored_refs != refs || status != "prepared" {
        return Err(Error::Denied);
    }
    for id in &stored_refs {
        vault.get_result(id).map_err(|_| Error::Worker("source_deleted".into()))?;
    }
    let (path, display): (String, String) = vault
        .db()
        .query_row("SELECT path,display_name FROM selections WHERE id=? AND kind='OutputSelectionRef' AND purpose='export' AND consumed=0", [&selection], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or(Error::Denied)?;
    let content = vault.get_blob(&blob)?;
    if sha256_hex(&content) != digest {
        return Err(Error::Integrity);
    }
    let target = std::path::PathBuf::from(&path);
    let parent = target.parent().ok_or(Error::Contract)?;
    let temporary = parent.join(format!(".lvjin-export-{}.tmp", uuid::Uuid::new_v4().simple()));
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&content)?;
        file.sync_all()?;
    }
    if target.exists() {
        std::fs::remove_file(&target)?;
    }
    std::fs::rename(&temporary, &target)?;
    if sha256_hex(&std::fs::read(&target)?) != digest {
        return Err(Error::Integrity);
    }
    vault.db().execute("UPDATE exports SET status='committed',committed_at=? WHERE id=?", params![now_rfc3339(), draft])?;
    vault.db().execute("UPDATE selections SET consumed=1 WHERE id=?", [&selection])?;
    let result = vault.put_result(invocation.skill(), invocation.id(), "ResultRef", &json!({"export": draft, "file_name": display, "bytes": content.len(), "sha256": digest, "format": format}))?;
    vault.commit_once(&key, invocation.root_id(), "export", Some(&draft), &json!({"result_ref": result}))?;
    vault.db().execute("INSERT INTO audit(root,invocation,code) VALUES(?,?,'export.committed')", params![invocation.root_id(), invocation.id()])?;
    Ok(completed(Some(result)))
}

pub fn diagnostics(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "diagnostics.redacted")?;
    let target = text_of(input, "target_ref")?;
    let detail = text_of(input, "detail")?;
    let layout = &ctx.services.layout;
    let vault = ctx.lock()?;
    let count = |sql: &str| -> i64 { vault.db().query_row(sql, [], |r| r.get(0)).unwrap_or(-1) };
    let mut report = json!({
        "target": target,
        "app_version": env!("CARGO_PKG_VERSION"),
        "milestone": "M1",
        "installation": {"production_layout": layout.production, "models": layout.artifacts.len(), "embedding_space_id": layout.space.id},
        "residency": {"max_large": ctx.services.residency.max_large, "coresident": ctx.services.residency.coresident},
        "local_worker_running": ctx.services.local.running(),
        "storage": {"matters": count("SELECT COUNT(*) FROM matters WHERE is_root=0"), "document_versions": count("SELECT COUNT(*) FROM document_versions"),
            "chunks": count("SELECT COUNT(*) FROM chunks"), "vectors": count("SELECT COUNT(*) FROM chunk_vectors"), "blobs": count("SELECT COUNT(*) FROM blobs"),
            "facts": count("SELECT COUNT(*) FROM facts"), "cashflows": count("SELECT COUNT(*) FROM cashflows"), "temp_store_memory": vault.temp_store_is_memory().unwrap_or(false)},
        "index": {"generation": count("SELECT COALESCE(MAX(generation),0) FROM index_state"), "loaded": vault.dense.is_some()},
        "network": "M1 makes no network requests; workers run in no-network AppContainers",
        "content": "no document text, names, paths, keys or passphrases are included",
    });
    if detail == "redacted_report" {
        let mut events = std::collections::BTreeMap::<String, i64>::new();
        if let Ok(entries) = std::fs::read_dir(ctx.services.log.directory()) {
            for entry in entries.flatten() {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    for line in text.lines().rev().take(5000) {
                        if let Ok(value) = serde_json::from_str::<Value>(line) {
                            if let Some(code) = value["event"].as_str() {
                                *events.entry(code.to_owned()).or_default() += 1;
                            }
                        }
                    }
                }
            }
        }
        report["event_counts"] = json!(events);
        report["installed_memory_bytes"] = json!(crate::model_pool::installed_memory_bytes());
        report["logical_processors"] = json!(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0));
    }
    let result = vault.put_result(invocation.skill(), invocation.id(), "DiagnosticsResultRef", &report)?;
    Ok(completed(Some(result)))
}
