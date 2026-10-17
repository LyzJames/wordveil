//! matter-manage (facts, cashflows, todos, relations, deletion) and
//! finance-calculate (design 7.3, 11, 9.8). Changes are prepared, previewed and
//! committed only by an explicit user confirmation root.
use super::*;
use crate::vault::now_rfc3339;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::time::Duration;

const RELATION_TYPES: &[&str] = &["cites", "amends", "supersedes", "evidence_for", "confirmed_fact", "derived_from"];
const FACT_TYPES: &[&str] = &["amount", "date", "party", "event", "region", "identity", "note"];
const KINDS: &[&str] = &["income", "expense", "transfer", "refund", "loan_out", "repayment", "fee", "interest", "principal_received", "payment"];
const MATTER_TYPES: &[&str] = &["rental", "loan", "employment", "household", "consumer", "insurance", "other"];

pub fn decimal_ok(text: &str) -> bool {
    let body = text.strip_prefix('-').unwrap_or(text);
    let mut parts = body.splitn(2, '.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next();
    !whole.is_empty() && whole.len() <= 15 && whole.bytes().all(|b| b.is_ascii_digit()) && fraction.is_none_or(|f| !f.is_empty() && f.len() <= 6 && f.bytes().all(|b| b.is_ascii_digit()))
}

pub fn normalize_amount(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !matches!(c, ',' | '，' | '¥' | '￥' | ' ' | '\u{3000}' | '+')).collect();
    let cleaned = cleaned.trim_end_matches('元').to_owned();
    if decimal_ok(&cleaned) { Some(cleaned) } else { None }
}

pub fn normalize_date(raw: &str) -> Option<String> {
    let text = raw.trim();
    let head: String = text.chars().take_while(|c| !c.is_whitespace() && *c != 'T').collect();
    for format in ["%Y-%m-%d", "%Y/%m/%d", "%Y.%m.%d", "%Y年%m月%d日", "%Y%m%d"] {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&head, format) {
            return Some(date.format("%Y-%m-%d").to_string());
        }
    }
    None
}

fn require(value: &Value, field: &str) -> Result<String> {
    value[field].as_str().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).ok_or(Error::Contract)
}

fn matter_of(tx: &Transaction, table: &str, id: &str) -> Result<String> {
    let sql = format!("SELECT t.matter_id FROM {table} t JOIN objects o ON o.id=t.id WHERE t.id=? AND o.active=1");
    tx.query_row(&sql, [id], |r| r.get(0)).optional()?.ok_or(Error::Denied)
}

fn register(tx: &Transaction, scope: &str, kind: &str, parent: Option<&str>) -> Result<String> {
    let id = crate::store::new_ref();
    tx.execute("INSERT INTO objects(id,scope,kind,version,active,created_at,parent) VALUES(?,?,?,'m1',1,?,?)", params![id, scope, kind, now_rfc3339(), parent])?;
    Ok(id)
}

fn deactivate(tx: &Transaction, id: &str) -> Result<()> {
    tx.execute("UPDATE objects SET active=0 WHERE id=?", [id])?;
    Ok(())
}

pub fn manage(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let matter = text_of(input, "matter_id")?;
    let operation = text_of(input, "operation")?;
    match operation.as_str() {
        "read" => read(ctx, invocation, &matter),
        "prepare_change" => prepare(ctx, invocation, &matter, input["change_ref"].as_str().ok_or(Error::Contract)?),
        "commit_change" => commit(ctx, invocation, &matter, input["change_ref"].as_str().ok_or(Error::Contract)?),
        _ => Err(Error::Contract),
    }
}

fn rows(ctx: &Ctx, sql: &str, args: &[&str], width: usize) -> Result<Vec<Vec<Value>>> {
    let vault = ctx.lock()?;
    let mut statement = vault.db().prepare(sql)?;
    let mapped = statement.query_map(rusqlite::params_from_iter(args.iter()), |r| {
        let mut row = vec![];
        for index in 0..width {
            let value: rusqlite::types::Value = r.get(index)?;
            row.push(match value {
                rusqlite::types::Value::Null => Value::Null,
                rusqlite::types::Value::Integer(v) => json!(v),
                rusqlite::types::Value::Real(v) => json!(v),
                rusqlite::types::Value::Text(v) => json!(v),
                rusqlite::types::Value::Blob(_) => Value::Null,
            });
        }
        Ok(row)
    })?;
    Ok(mapped.collect::<std::result::Result<_, _>>()?)
}

fn parse_json(value: &Value) -> Value {
    value.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null)
}

fn read(ctx: &Ctx, invocation: &Invocation, matter: &str) -> Result<EnvelopeParts> {
    let info = rows(ctx, "SELECT m.id,m.title,m.matter_type,m.region,m.status,m.is_root,m.notes,m.created_at,m.updated_at FROM matters m JOIN objects o ON o.id=m.id WHERE m.id=? AND o.active=1", &[matter], 9)?;
    let info = info.first().ok_or(Error::Denied)?;
    let names = ["id", "title", "matter_type", "region", "status", "is_root", "notes", "created_at", "updated_at"];
    let matter_json: Value = names.iter().zip(info.iter()).map(|(k, v)| ((*k).to_string(), v.clone())).collect::<serde_json::Map<_, _>>().into();
    let is_root = info[5] == json!(1);
    let mut payload = json!({"matter": matter_json});
    if is_root {
        let list = rows(ctx, "SELECT m.id,m.title,m.matter_type,m.status,m.updated_at,(SELECT COUNT(*) FROM documents d WHERE d.matter_id=m.id),(SELECT COUNT(*) FROM facts f WHERE f.matter_id=m.id AND f.status='candidate') FROM matters m JOIN objects o ON o.id=m.id WHERE m.is_root=0 AND o.active=1 ORDER BY m.updated_at DESC", &[], 7)?;
        payload["matters"] = json!(list.iter().map(|r| json!({"id": r[0], "title": r[1], "matter_type": r[2], "status": r[3], "updated_at": r[4], "documents": r[5], "candidate_facts": r[6]})).collect::<Vec<_>>());
    }
    let versions = rows(ctx,
        "SELECT d.id,d.display_name,v.id,v.version_no,v.media_kind,v.status,v.page_count,v.bytes,v.created_at,v.error_code,(SELECT m.id FROM media m WHERE m.version_id=v.id) FROM documents d JOIN document_versions v ON v.document_id=d.id JOIN objects o ON o.id=v.id WHERE d.matter_id=? AND o.active=1 ORDER BY d.created_at,v.version_no",
        &[matter], 11)?;
    payload["documents"] = json!(versions.iter().map(|r| json!({"document_id": r[0], "display_name": r[1], "version_id": r[2], "version_no": r[3], "media_kind": r[4], "status": r[5], "pages": r[6], "bytes": r[7], "created_at": r[8], "error_code": r[9], "media_id": r[10]})).collect::<Vec<_>>());
    let facts = rows(ctx, "SELECT f.id,f.fact_type,f.value,f.label,f.status,f.origin,f.source_version,f.source_locator,f.created_at FROM facts f JOIN objects o ON o.id=f.id WHERE f.matter_id=? AND o.active=1 ORDER BY f.created_at LIMIT 5000", &[matter], 9)?;
    payload["facts"] = json!(facts.iter().map(|r| json!({"id": r[0], "fact_type": r[1], "value": parse_json(&r[2]), "label": r[3], "status": r[4], "origin": r[5], "source_version": r[6], "source": parse_json(&r[7]), "created_at": r[8]})).collect::<Vec<_>>());
    let cash = rows(ctx, "SELECT c.id,c.occurred_on,c.amount,c.currency,c.direction,c.kind,c.category,c.counterparty,c.description,c.status,c.account_id,c.source_version,c.source_locator FROM cashflows c JOIN objects o ON o.id=c.id WHERE c.matter_id=? AND o.active=1 ORDER BY c.occurred_on,c.created_at LIMIT 10000", &[matter], 13)?;
    payload["cashflows"] = json!(cash.iter().map(|r| json!({"id": r[0], "date": r[1], "amount": r[2], "currency": r[3], "direction": r[4], "kind": r[5], "category": r[6], "counterparty": r[7], "description": r[8], "status": r[9], "account_id": r[10], "source_version": r[11], "source": parse_json(&r[12])})).collect::<Vec<_>>());
    let accounts = rows(ctx, "SELECT a.id,a.name,a.currency FROM accounts a JOIN objects o ON o.id=a.id WHERE a.matter_id=? AND o.active=1", &[matter], 3)?;
    payload["accounts"] = json!(accounts.iter().map(|r| json!({"id": r[0], "name": r[1], "currency": r[2]})).collect::<Vec<_>>());
    let todos = rows(ctx, "SELECT t.id,t.title,t.due_on,t.status,t.source_fact FROM todos t JOIN objects o ON o.id=t.id WHERE t.matter_id=? AND o.active=1 ORDER BY t.due_on", &[matter], 5)?;
    payload["todos"] = json!(todos.iter().map(|r| json!({"id": r[0], "title": r[1], "due_on": r[2], "status": r[3], "source_fact": r[4]})).collect::<Vec<_>>());
    let relations = rows(ctx, "SELECT r.id,r.src,r.dst,r.relation_type,r.evidence,r.origin,r.status,r.created_at FROM relations r JOIN objects o ON o.id=r.id WHERE r.matter_id=? AND o.active=1", &[matter], 8)?;
    payload["relations"] = json!(relations.iter().map(|r| json!({"id": r[0], "src": r[1], "dst": r[2], "relation_type": r[3], "evidence": parse_json(&r[4]), "origin": r[5], "status": r[6], "created_at": r[7]})).collect::<Vec<_>>());
    let assumptions = rows(ctx, "SELECT a.id,a.kind,a.value FROM assumptions a JOIN objects o ON o.id=a.id WHERE a.matter_id=? AND o.active=1 ORDER BY a.created_at", &[matter], 3)?;
    payload["assumptions"] = json!(assumptions.iter().map(|r| json!({"id": r[0], "kind": r[1], "value": parse_json(&r[2])})).collect::<Vec<_>>());
    let calcs = rows(ctx, "SELECT r.id,json_extract(r.payload,'$.calculation_type'),r.created_at FROM results r JOIN objects o ON o.id=r.id WHERE r.kind='CalculationRef' AND o.active=1 AND json_extract(r.payload,'$.matter_id')=? ORDER BY r.created_at DESC LIMIT 50", &[matter], 3)?;
    payload["calculations"] = json!(calcs.iter().map(|r| json!({"id": r[0], "calculation_type": r[1], "created_at": r[2]})).collect::<Vec<_>>());
    let mut timeline: Vec<Value> = vec![];
    for fact in payload["facts"].as_array().cloned().unwrap_or_default() {
        if let Some(date) = fact["value"]["date"].as_str() {
            timeline.push(json!({"date": date, "kind": "fact", "ref": fact["id"], "label": fact["label"], "status": fact["status"]}));
        }
    }
    for row in payload["cashflows"].as_array().cloned().unwrap_or_default() {
        timeline.push(json!({"date": row["date"], "kind": "cashflow", "ref": row["id"], "label": format!("{} {} {}", if row["direction"] == "in" { "收入" } else { "支出" }, row["amount"].as_str().unwrap_or(""), row["counterparty"].as_str().unwrap_or("")), "status": row["status"]}));
    }
    for todo in payload["todos"].as_array().cloned().unwrap_or_default() {
        if let Some(due) = todo["due_on"].as_str() {
            timeline.push(json!({"date": due, "kind": "todo", "ref": todo["id"], "label": todo["title"], "status": todo["status"]}));
        }
    }
    timeline.sort_by(|a, b| a["date"].as_str().cmp(&b["date"].as_str()));
    payload["timeline"] = json!(timeline);
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "MatterSnapshotRef", &payload)?;
    Ok(completed(Some(result)))
}

/// Validate a draft change against this matter and compute its impact preview.
fn prepare(ctx: &Ctx, invocation: &Invocation, matter: &str, change_ref: &str) -> Result<EnvelopeParts> {
    let (change_matter, change, status): (String, String, String) = ctx
        .lock()?
        .db()
        .query_row("SELECT c.matter_id,c.change,c.status FROM changes c JOIN objects o ON o.id=c.id WHERE c.id=? AND o.active=1", [change_ref], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?
        .ok_or(Error::Denied)?;
    if change_matter != matter || !matches!(status.as_str(), "draft" | "prepared") {
        return Err(Error::Denied);
    }
    let change: Value = serde_json::from_str(&change)?;
    let impact = {
        let mut vault = ctx.lock()?;
        let tx = vault.runtime.ledger.db.transaction()?;
        let impact = validate(&tx, matter, &change)?;
        tx.execute("UPDATE changes SET status='prepared',impact=? WHERE id=?", params![impact.to_string(), change_ref])?;
        tx.commit()?;
        impact
    };
    let payload = json!({"change_ref": change_ref, "matter_id": matter, "change": change, "impact": impact, "status": "prepared"});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ChangeResultRef", &payload)?;
    Ok(completed(Some(result)))
}

fn validate(tx: &Transaction, matter: &str, change: &Value) -> Result<Value> {
    let is_root: i64 = tx.query_row("SELECT is_root FROM matters WHERE id=?", [matter], |r| r.get(0))?;
    let kind = change["type"].as_str().ok_or(Error::Contract)?;
    let root_only = matches!(kind, "create_matter");
    if root_only != (is_root == 1) && kind != "set_setting" {
        return Err(Error::Denied);
    }
    match kind {
        "create_matter" => {
            let title = require(change, "title")?;
            let matter_type = require(change, "matter_type")?;
            if title.chars().count() > 120 || !MATTER_TYPES.contains(&matter_type.as_str()) {
                return Err(Error::Contract);
            }
            Ok(json!({"creates": "matter", "title": title}))
        }
        "update_matter" => Ok(json!({"updates": "matter"})),
        "add_fact" => {
            let fact_type = require(change, "fact_type")?;
            if !FACT_TYPES.contains(&fact_type.as_str()) {
                return Err(Error::Contract);
            }
            check_fact_value(&fact_type, &change["value"])?;
            Ok(json!({"creates": "fact"}))
        }
        "confirm_facts" | "reject_facts" => {
            let ids = id_list(change, "fact_ids")?;
            for id in &ids {
                if matter_of(tx, "facts", id)? != matter {
                    return Err(Error::Denied);
                }
            }
            Ok(json!({"facts": ids.len()}))
        }
        "edit_fact" => {
            let id = require(change, "fact_id")?;
            if matter_of(tx, "facts", &id)? != matter {
                return Err(Error::Denied);
            }
            let fact_type: String = tx.query_row("SELECT fact_type FROM facts WHERE id=?", [&id], |r| r.get(0))?;
            check_fact_value(&fact_type, &change["value"])?;
            Ok(json!({"edits": id}))
        }
        "add_cashflow" => {
            check_cashflow(change)?;
            Ok(json!({"creates": "cashflow"}))
        }
        "confirm_cashflows" | "reject_cashflows" => {
            let ids = id_list(change, "cashflow_ids")?;
            for id in &ids {
                if matter_of(tx, "cashflows", id)? != matter {
                    return Err(Error::Denied);
                }
            }
            Ok(json!({"cashflows": ids.len()}))
        }
        "import_cashflows_from_table" => {
            let version = require(change, "version_id")?;
            let owner: String = tx.query_row("SELECT d.matter_id FROM document_versions v JOIN documents d ON d.id=v.document_id JOIN objects o ON o.id=v.id WHERE v.id=? AND o.active=1", [&version], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
            if owner != matter {
                return Err(Error::Denied);
            }
            let parsed = table_rows(tx, &version, change)?;
            let errors = parsed.iter().filter(|r| r.get("error").is_some()).count();
            Ok(json!({"rows": parsed.len(), "rows_with_errors": errors, "preview": parsed.iter().take(20).collect::<Vec<_>>()}))
        }
        "add_account" => {
            require(change, "name")?;
            let currency = require(change, "currency")?;
            if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
                return Err(Error::Contract);
            }
            Ok(json!({"creates": "account"}))
        }
        "add_todo" => {
            require(change, "title")?;
            if let Some(due) = change["due_on"].as_str() {
                normalize_date(due).ok_or(Error::Contract)?;
            }
            Ok(json!({"creates": "todo"}))
        }
        "complete_todo" => {
            let id = require(change, "todo_id")?;
            if matter_of(tx, "todos", &id)? != matter {
                return Err(Error::Denied);
            }
            Ok(json!({"completes": id}))
        }
        "add_relation" => {
            let kind = require(change, "relation_type")?;
            if !RELATION_TYPES.contains(&kind.as_str()) {
                return Err(Error::Contract);
            }
            let src = require(change, "src")?;
            let dst = require(change, "dst")?;
            for node in [&src, &dst] {
                let active: Option<String> = tx.query_row("SELECT kind FROM objects WHERE id=? AND active=1", [node], |r| r.get(0)).optional()?;
                let kind = active.ok_or(Error::Denied)?;
                if !matches!(kind.as_str(), "DocumentVersionRef" | "EvidenceRef" | "FactRef" | "FinancialInputRef" | "PageRef" | "MatterRef") {
                    return Err(Error::Contract);
                }
            }
            Ok(json!({"creates": "relation", "relation_type": kind}))
        }
        "remove_relation" => {
            let id = require(change, "relation_id")?;
            if matter_of(tx, "relations", &id)? != matter {
                return Err(Error::Denied);
            }
            Ok(json!({"removes": id}))
        }
        "set_assumptions" => {
            let values = change["values"].as_object().ok_or(Error::Contract)?;
            if values.is_empty() || values.len() > 20 {
                return Err(Error::Contract);
            }
            for key in values.keys() {
                if !matches!(key.as_str(), "rounding" | "day_count" | "currency_precision" | "period_start" | "period_end" | "exclude_kinds" | "refund_policy" | "loan_mode" | "initial_balance" | "scenario" | "anomaly_threshold") {
                    return Err(Error::Contract);
                }
            }
            Ok(json!({"creates": "assumptions", "keys": values.keys().collect::<Vec<_>>()}))
        }
        "delete_document_version" => {
            let version = require(change, "version_id")?;
            let owner: String = tx.query_row("SELECT d.matter_id FROM document_versions v JOIN documents d ON d.id=v.document_id JOIN objects o ON o.id=v.id WHERE v.id=? AND o.active=1", [&version], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
            if owner != matter {
                return Err(Error::Denied);
            }
            deletion_impact(tx, &version)
        }
        "delete_matter" => {
            let versions: i64 = tx.query_row("SELECT COUNT(*) FROM documents d JOIN document_versions v ON v.document_id=d.id JOIN objects o ON o.id=v.id WHERE d.matter_id=? AND o.active=1", [matter], |r| r.get(0))?;
            Ok(json!({"deletes": "matter", "document_versions": versions}))
        }
        _ => Err(Error::Contract),
    }
}

fn id_list(change: &Value, field: &str) -> Result<Vec<String>> {
    let ids: Vec<String> = change[field].as_array().ok_or(Error::Contract)?.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    if ids.is_empty() || ids.len() > 5000 {
        return Err(Error::Contract);
    }
    Ok(ids)
}

fn check_fact_value(fact_type: &str, value: &Value) -> Result<()> {
    match fact_type {
        "amount" => {
            let amount = value["amount"].as_str().ok_or(Error::Contract)?;
            let currency = value["currency"].as_str().unwrap_or("CNY");
            if !decimal_ok(amount) || amount.starts_with('-') || currency.len() != 3 {
                return Err(Error::Contract);
            }
        }
        "date" => {
            normalize_date(value["date"].as_str().ok_or(Error::Contract)?).ok_or(Error::Contract)?;
        }
        _ => {
            let text = value["text"].as_str().ok_or(Error::Contract)?;
            if text.trim().is_empty() || text.chars().count() > 2000 {
                return Err(Error::Contract);
            }
        }
    }
    Ok(())
}

fn check_cashflow(change: &Value) -> Result<()> {
    normalize_date(&require(change, "date")?).ok_or(Error::Contract)?;
    let amount = require(change, "amount")?;
    if !decimal_ok(&amount) || amount.starts_with('-') {
        return Err(Error::Contract);
    }
    let direction = require(change, "direction")?;
    let kind = require(change, "kind")?;
    let currency = change["currency"].as_str().unwrap_or("CNY");
    if !matches!(direction.as_str(), "in" | "out") || !KINDS.contains(&kind.as_str()) || currency.len() != 3 {
        return Err(Error::Contract);
    }
    Ok(())
}

/// Parse stored table rows with an explicit user column mapping.
fn table_rows(tx: &Transaction, version: &str, change: &Value) -> Result<Vec<Value>> {
    let mapping = change["mapping"].as_object().ok_or(Error::Contract)?;
    let column = |name: &str| mapping.get(name).and_then(Value::as_u64).map(|v| v as usize);
    let date_col = column("date").ok_or(Error::Contract)?;
    let amount_col = column("amount");
    let income_col = column("income");
    let expense_col = column("expense");
    if amount_col.is_none() && income_col.is_none() && expense_col.is_none() {
        return Err(Error::Contract);
    }
    let rules: Vec<(String, String)> = change["kind_rules"].as_array().cloned().unwrap_or_default().iter()
        .filter_map(|r| Some((r["contains"].as_str()?.to_owned(), r["kind"].as_str().filter(|k| KINDS.contains(k))?.to_owned())))
        .collect();
    let currency_default = change["currency"].as_str().unwrap_or("CNY").to_owned();
    let mut statement = tx.prepare("SELECT rowid,locator FROM blocks WHERE version_id=? AND kind='sheet_row' ORDER BY ordinal")?;
    let rows = statement.query_map([version], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    let mut output = vec![];
    for row in rows {
        let (rowid, locator) = row?;
        let locator: Value = serde_json::from_str(&locator)?;
        let cells = locator["cells"].as_array().cloned().unwrap_or_default();
        let cell = |index: Option<usize>| -> Option<String> {
            let index = index?;
            cells.iter().find(|c| c["col"].as_u64() == Some(index as u64)).or_else(|| cells.get(index.saturating_sub(1))).and_then(|c| c["value"].as_str()).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
        };
        let text_of_row: String = cells.iter().filter_map(|c| c["value"].as_str()).collect::<Vec<_>>().join(" ");
        let date = cell(Some(date_col)).and_then(|d| normalize_date(&d));
        let (amount, direction) = if let Some(raw) = cell(amount_col) {
            match normalize_amount(&raw) {
                Some(value) if value.starts_with('-') => (Some(value[1..].to_owned()), Some("out")),
                Some(value) => (Some(value), cell(column("direction")).map(|d| if d.contains('支') || d.contains("出") { "out" } else { "in" })),
                None => (None, None),
            }
        } else if let Some(raw) = cell(income_col).and_then(|v| normalize_amount(&v)).filter(|v| v != "0" && v != "0.00") {
            (Some(raw), Some("in"))
        } else if let Some(raw) = cell(expense_col).and_then(|v| normalize_amount(&v)).filter(|v| v != "0" && v != "0.00") {
            (Some(raw), Some("out"))
        } else {
            (None, None)
        };
        let mut kind = match direction {
            Some("in") => "income",
            _ => "expense",
        }
        .to_owned();
        for (needle, mapped) in &rules {
            if text_of_row.contains(needle.as_str()) {
                kind = mapped.clone();
                break;
            }
        }
        let mut item = json!({"block": rowid, "row": locator["row"], "date": date, "amount": amount, "direction": direction, "kind": kind,
            "counterparty": cell(column("counterparty")), "description": cell(column("description")), "category": cell(column("category")),
            "currency": cell(column("currency")).filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase())).unwrap_or_else(|| currency_default.clone())});
        if date.is_none() || amount.is_none() || direction.is_none() {
            item["error"] = json!("row_not_a_transaction");
        }
        output.push(item);
    }
    Ok(output)
}

fn deletion_impact(tx: &Transaction, version: &str) -> Result<Value> {
    let count = |sql: &str| -> Result<i64> { Ok(tx.query_row(sql, [version], |r| r.get(0))?) };
    Ok(json!({
        "deletes": "document_version",
        "chunks": count("SELECT COUNT(*) FROM chunks WHERE version_id=?")?,
        "blocks": count("SELECT COUNT(*) FROM blocks WHERE version_id=?")?,
        "pages": count("SELECT COUNT(*) FROM pages WHERE version_id=?")?,
        "facts": count("SELECT COUNT(*) FROM facts WHERE source_version=?")?,
        "confirmed_facts": count("SELECT COUNT(*) FROM facts WHERE source_version=? AND status='confirmed'")?,
        "cashflows": count("SELECT COUNT(*) FROM cashflows WHERE source_version=?")?,
        "media": count("SELECT COUNT(*) FROM media WHERE version_id=?")?,
        "note": "原件、派生图、文字、全文与向量索引、关系、相关检索快照和导出草稿将一并删除；已有备份不受影响，普通删除不能保证擦除硬盘物理残留。",
    }))
}

fn commit(ctx: &Ctx, invocation: &Invocation, matter: &str, change_ref: &str) -> Result<EnvelopeParts> {
    {
        let vault = ctx.lock()?;
        if vault.runtime.root_action(invocation.root_id())? != "confirm" {
            return Err(Error::Denied);
        }
    }
    ctx.authorize(invocation, "vault.write")?;
    let key = format!("change:{change_ref}");
    if let Some(outcome) = ctx.lock()?.db().query_row("SELECT outcome FROM commit_log WHERE idempotency_key=?", [&key], |r| r.get::<_, String>(0)).optional()? {
        let outcome: Value = serde_json::from_str(&outcome)?;
        let mut parts = completed(outcome["result_ref"].as_str().map(str::to_owned));
        warn(&mut parts, "already_committed", "该变更已提交过，未重复执行。");
        return Ok(parts);
    }
    let (change_matter, change, status): (String, String, String) = ctx
        .lock()?
        .db()
        .query_row("SELECT c.matter_id,c.change,c.status FROM changes c JOIN objects o ON o.id=c.id WHERE c.id=? AND o.active=1", [change_ref], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?
        .ok_or(Error::Denied)?;
    if change_matter != matter || status != "prepared" {
        return Err(Error::Denied);
    }
    let change: Value = serde_json::from_str(&change)?;
    let kind = change["type"].as_str().unwrap_or("").to_owned();
    if matches!(kind.as_str(), "add_relation" | "remove_relation" | "confirm_facts") {
        // Confirmed relation writes (including the evidence edge of a confirmed
        // candidate) need the conditional graph.write capability.
        ctx.authorize(invocation, "graph.write")?;
    }
    let mut rebuild_index = false;
    let created = {
        let mut vault = ctx.lock()?;
        let scope = vault.id.clone();
        let mut removed_blobs: Vec<String> = vec![];
        let tx = vault.runtime.ledger.db.transaction()?;
        validate(&tx, matter, &change)?;
        let created = apply(&tx, &scope, matter, &change, &mut removed_blobs, &mut rebuild_index)?;
        tx.execute("UPDATE changes SET status='committed',committed_at=? WHERE id=?", params![now_rfc3339(), change_ref])?;
        tx.execute("UPDATE matters SET updated_at=? WHERE id=?", params![now_rfc3339(), matter])?;
        tx.commit()?;
        for blob in removed_blobs {
            vault.delete_blob(&blob)?;
        }
        created
    };
    if rebuild_index {
        // Rebuild from the remaining encrypted vectors; the old snapshot (which
        // still contains deleted vectors) is replaced and its blob removed.
        {
            let mut vault = ctx.lock()?;
            vault.db().execute("UPDATE index_state SET dirty=1 WHERE id=1", [])?;
            vault.dense = None;
        }
        super::indexing::ensure_dense(ctx)?;
    }
    let payload = json!({"change_ref": change_ref, "matter_id": matter, "type": kind, "created": created, "status": "committed"});
    let vault = ctx.lock()?;
    let result = vault.put_result(invocation.skill(), invocation.id(), "ChangeResultRef", &payload)?;
    vault.commit_once(&key, invocation.root_id(), "matter_change", Some(change_ref), &json!({"result_ref": result}))?;
    vault.db().execute("INSERT INTO audit(root,invocation,code) VALUES(?,?,?)", params![invocation.root_id(), invocation.id(), format!("change.committed.{kind}")])?;
    let mut parts = completed(Some(result));
    parts.evidence_refs = created.as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    Ok(parts)
}

fn apply(tx: &Transaction, scope: &str, matter: &str, change: &Value, removed: &mut Vec<String>, rebuild: &mut bool) -> Result<Value> {
    let now = now_rfc3339();
    let mut created = vec![];
    match change["type"].as_str().unwrap_or("") {
        "create_matter" => {
            let id = register(tx, scope, "MatterRef", None)?;
            tx.execute(
                "INSERT INTO matters(id,title,matter_type,region,status,is_root,notes,created_at,updated_at) VALUES(?,?,?,?,'active',0,?,?,?)",
                params![id, require(change, "title")?, require(change, "matter_type")?, change["region"].as_str(), change["notes"].as_str(), now, now],
            )?;
            created.push(id);
        }
        "update_matter" => {
            for field in ["title", "region", "status", "notes"] {
                if let Some(value) = change[field].as_str() {
                    if field == "status" && !matches!(value, "active" | "closed" | "archived") {
                        return Err(Error::Contract);
                    }
                    tx.execute(&format!("UPDATE matters SET {field}=? WHERE id=?"), params![value, matter])?;
                }
            }
        }
        "add_fact" => {
            let id = register(tx, scope, "FactRef", Some(matter))?;
            tx.execute(
                "INSERT INTO facts(id,matter_id,fact_type,value,label,source_version,source_locator,status,origin,created_at,confirmed_at) VALUES(?,?,?,?,?,?,?,'confirmed','user',?,?)",
                params![id, matter, require(change, "fact_type")?, change["value"].to_string(), change["label"].as_str(), change["source_version"].as_str(), change.get("source").map(Value::to_string), now, now],
            )?;
            created.push(id);
        }
        "confirm_facts" => {
            for id in id_list(change, "fact_ids")? {
                tx.execute("UPDATE facts SET status='confirmed',confirmed_at=? WHERE id=?", params![now, id])?;
                // A confirmed rule candidate keeps an explicit evidence edge to its source version.
                let source: Option<String> = tx.query_row("SELECT source_version FROM facts WHERE id=?", [&id], |r| r.get(0))?;
                if let Some(version) = source {
                    let edge = register(tx, scope, "EdgeRef", Some(&id))?;
                    let evidence: Option<String> = tx.query_row("SELECT source_locator FROM facts WHERE id=?", [&id], |r| r.get(0))?;
                    tx.execute("INSERT INTO relations(id,matter_id,src,dst,relation_type,evidence,origin,status,created_at) VALUES(?,?,?,?,'evidence_for',?,'user_confirmed_candidate','confirmed',?)",
                        params![edge, matter, version, id, evidence, now])?;
                }
            }
        }
        "reject_facts" => {
            for id in id_list(change, "fact_ids")? {
                tx.execute("UPDATE facts SET status='rejected' WHERE id=?", [&id])?;
            }
        }
        "edit_fact" => {
            let id = require(change, "fact_id")?;
            tx.execute("UPDATE facts SET value=?,label=COALESCE(?,label),status='confirmed',origin=CASE WHEN origin='rule_extracted' THEN 'rule_corrected' ELSE origin END,confirmed_at=? WHERE id=?",
                params![change["value"].to_string(), change["label"].as_str(), now, id])?;
        }
        "add_cashflow" => {
            let id = register(tx, scope, "FinancialInputRef", Some(matter))?;
            tx.execute(
                "INSERT INTO cashflows(id,matter_id,account_id,occurred_on,amount,currency,direction,kind,category,counterparty,description,source_version,source_locator,status,created_at,confirmed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,'confirmed',?,?)",
                params![id, matter, change["account_id"].as_str(), normalize_date(&require(change, "date")?), require(change, "amount")?, change["currency"].as_str().unwrap_or("CNY"),
                    require(change, "direction")?, require(change, "kind")?, change["category"].as_str(), change["counterparty"].as_str(), change["description"].as_str(),
                    change["source_version"].as_str(), change.get("source").map(Value::to_string), now, now],
            )?;
            created.push(id);
        }
        "confirm_cashflows" => {
            for id in id_list(change, "cashflow_ids")? {
                tx.execute("UPDATE cashflows SET status='confirmed',confirmed_at=? WHERE id=? AND status='candidate'", params![now, id])?;
            }
        }
        "reject_cashflows" => {
            for id in id_list(change, "cashflow_ids")? {
                tx.execute("UPDATE cashflows SET status='rejected' WHERE id=?", [&id])?;
            }
        }
        "import_cashflows_from_table" => {
            let version = require(change, "version_id")?;
            let stale: Vec<String> = {
                let mut statement = tx.prepare("SELECT id FROM cashflows WHERE source_version=? AND status='candidate'")?;
                let rows = statement.query_map([&version], |r| r.get(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            for id in stale {
                tx.execute("DELETE FROM cashflows WHERE id=?", [&id])?;
                deactivate(tx, &id)?;
            }
            for row in table_rows(tx, &version, change)? {
                if row.get("error").is_some() {
                    continue;
                }
                let id = register(tx, scope, "FinancialInputRef", Some(&version))?;
                tx.execute(
                    "INSERT INTO cashflows(id,matter_id,account_id,occurred_on,amount,currency,direction,kind,category,counterparty,description,source_version,source_locator,status,dedup_key,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,'candidate',?,?)",
                    params![id, matter, change["account_id"].as_str(), row["date"].as_str(), row["amount"].as_str(), row["currency"].as_str(), row["direction"].as_str(), row["kind"].as_str(),
                        row["category"].as_str(), row["counterparty"].as_str(), row["description"].as_str(), version, json!({"block": row["block"], "row": row["row"]}).to_string(),
                        format!("{version}:{}", row["row"]), now],
                )?;
                created.push(id);
            }
        }
        "add_account" => {
            let id = register(tx, scope, "AccountRef", Some(matter))?;
            tx.execute("INSERT INTO accounts(id,matter_id,name,currency,created_at) VALUES(?,?,?,?,?)", params![id, matter, require(change, "name")?, require(change, "currency")?, now])?;
            created.push(id);
        }
        "add_todo" => {
            let id = register(tx, scope, "TodoRef", Some(matter))?;
            tx.execute("INSERT INTO todos(id,matter_id,title,due_on,status,source_fact,created_at) VALUES(?,?,?,?,'open',?,?)",
                params![id, matter, require(change, "title")?, change["due_on"].as_str().and_then(normalize_date), change["source_fact"].as_str(), now])?;
            created.push(id);
        }
        "complete_todo" => {
            tx.execute("UPDATE todos SET status='done' WHERE id=?", [require(change, "todo_id")?])?;
        }
        "add_relation" => {
            let id = register(tx, scope, "EdgeRef", Some(matter))?;
            tx.execute("INSERT INTO relations(id,matter_id,src,dst,relation_type,evidence,origin,status,created_at) VALUES(?,?,?,?,?,?,'user','confirmed',?)",
                params![id, matter, require(change, "src")?, require(change, "dst")?, require(change, "relation_type")?, change.get("evidence").map(Value::to_string), now])?;
            created.push(id);
        }
        "remove_relation" => {
            let id = require(change, "relation_id")?;
            tx.execute("DELETE FROM relations WHERE id=?", [&id])?;
            deactivate(tx, &id)?;
        }
        "set_assumptions" => {
            for (key, value) in change["values"].as_object().ok_or(Error::Contract)? {
                let id = register(tx, scope, "AssumptionRef", Some(matter))?;
                tx.execute("INSERT INTO assumptions(id,matter_id,kind,value,created_at) VALUES(?,?,?,?,?)", params![id, matter, key, value.to_string(), now])?;
                created.push(id);
            }
        }
        "delete_document_version" => {
            delete_version(tx, &require(change, "version_id")?, removed)?;
            *rebuild = true;
        }
        "delete_matter" => {
            let versions: Vec<String> = {
                let mut statement = tx.prepare("SELECT v.id FROM documents d JOIN document_versions v ON v.document_id=d.id WHERE d.matter_id=?")?;
                let rows = statement.query_map([matter], |r| r.get(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            for version in versions {
                delete_version(tx, &version, removed)?;
            }
            for table in ["facts", "cashflows", "accounts", "todos", "relations", "assumptions", "changes"] {
                let ids: Vec<String> = {
                    let mut statement = tx.prepare(&format!("SELECT id FROM {table} WHERE matter_id=?"))?;
                    let rows = statement.query_map([matter], |r| r.get(0))?;
                    rows.collect::<std::result::Result<_, _>>()?
                };
                for id in ids {
                    tx.execute(&format!("DELETE FROM {table} WHERE id=?"), [&id])?;
                    deactivate(tx, &id)?;
                }
            }
            tx.execute("DELETE FROM documents WHERE matter_id=?", [matter])?;
            deactivate(tx, matter)?;
            *rebuild = true;
        }
        _ => return Err(Error::Contract),
    }
    Ok(json!(created))
}

/// Deletion propagation (design 9.8): original, derived images and maps, text,
/// full-text and vector rows, facts/cashflows/relations from the version,
/// retrieval/evidence/export snapshots that reference it.
pub fn delete_version(tx: &Transaction, version: &str, removed: &mut Vec<String>) -> Result<()> {
    let mut objects: Vec<String> = vec![version.to_owned()];
    let chunk_rows: Vec<(i64, String)> = {
        let mut statement = tx.prepare("SELECT rowid,id FROM chunks WHERE version_id=?")?;
        let rows = statement.query_map([version], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (rowid, id) in &chunk_rows {
        tx.execute("DELETE FROM chunk_fts WHERE rowid=?", [rowid])?;
        tx.execute("DELETE FROM chunk_vectors WHERE chunk_rowid=?", [rowid])?;
        objects.push(id.clone());
    }
    tx.execute("DELETE FROM chunks WHERE version_id=?", [version])?;
    tx.execute("DELETE FROM blocks WHERE version_id=?", [version])?;
    let pages: Vec<String> = {
        let mut statement = tx.prepare("SELECT id FROM pages WHERE version_id=?")?;
        let rows = statement.query_map([version], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let media: Vec<String> = {
        let mut statement = tx.prepare("SELECT id FROM media WHERE version_id=?")?;
        let rows = statement.query_map([version], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for media_id in &media {
        let segments: Vec<String> = {
            let mut statement = tx.prepare("SELECT id FROM audio_segments WHERE media_id=?")?;
            let rows = statement.query_map([media_id], |r| r.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        objects.extend(segments.iter().cloned());
        let frames: Vec<String> = {
            let mut statement = tx.prepare("SELECT id FROM frames WHERE media_id=?")?;
            let rows = statement.query_map([media_id], |r| r.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        objects.extend(frames.iter().cloned());
        tx.execute("DELETE FROM audio_segments WHERE media_id=?", [media_id])?;
        tx.execute("DELETE FROM frames WHERE media_id=?", [media_id])?;
        objects.push(media_id.clone());
    }
    tx.execute("DELETE FROM media WHERE version_id=?", [version])?;
    objects.extend(pages.iter().cloned());
    tx.execute("DELETE FROM pages WHERE version_id=?", [version])?;
    let facts: Vec<String> = {
        let mut statement = tx.prepare("SELECT id FROM facts WHERE source_version=?")?;
        let rows = statement.query_map([version], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let cash: Vec<String> = {
        let mut statement = tx.prepare("SELECT id FROM cashflows WHERE source_version=?")?;
        let rows = statement.query_map([version], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    tx.execute("DELETE FROM facts WHERE source_version=?", [version])?;
    tx.execute("DELETE FROM cashflows WHERE source_version=?", [version])?;
    objects.extend(facts.iter().cloned());
    objects.extend(cash.iter().cloned());
    let linked: Vec<String> = {
        let mut statement = tx.prepare("SELECT id,src,dst FROM relations")?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut ids = vec![];
        for row in rows {
            let (id, src, dst) = row?;
            if objects.contains(&src) || objects.contains(&dst) {
                ids.push(id);
            }
        }
        ids
    };
    for id in &linked {
        tx.execute("DELETE FROM relations WHERE id=?", [id])?;
        objects.push(id.clone());
    }
    // Snapshots (results) that quote this version or its evidence are removed.
    let results: Vec<(String, String)> = {
        let mut statement = tx.prepare("SELECT id,payload FROM results")?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let needles: Vec<&String> = objects.iter().collect();
    let mut stale_results = vec![];
    for (id, payload) in &results {
        if needles.iter().any(|needle| payload.contains(needle.as_str())) {
            stale_results.push(id.clone());
        }
    }
    for id in &stale_results {
        tx.execute("DELETE FROM results WHERE id=?", [id])?;
        objects.push(id.clone());
    }
    let exports: Vec<(String, String, String)> = {
        let mut statement = tx.prepare("SELECT id,result_refs,content_blob FROM exports")?;
        let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (id, refs, blob) in exports {
        if stale_results.iter().any(|r| refs.contains(r.as_str())) {
            tx.execute("DELETE FROM exports WHERE id=?", [&id])?;
            removed.push(blob);
            objects.push(id);
        }
    }
    // Every blob owned by a removed object (original, page images, PCM, drafts).
    let blobs: Vec<(String, String)> = {
        let mut statement = tx.prepare("SELECT id,owner FROM blobs")?;
        let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (blob, owner) in blobs {
        if objects.contains(&owner) {
            removed.push(blob);
        }
    }
    let document: Option<String> = tx.query_row("SELECT document_id FROM document_versions WHERE id=?", [version], |r| r.get(0)).optional()?;
    tx.execute("DELETE FROM document_versions WHERE id=?", [version])?;
    tx.execute("DELETE FROM jobs WHERE id=?", [version])?;
    if let Some(document) = document {
        let remaining: i64 = tx.query_row("SELECT COUNT(*) FROM document_versions WHERE document_id=?", [&document], |r| r.get(0))?;
        if remaining == 0 {
            tx.execute("DELETE FROM documents WHERE id=?", [&document])?;
        }
    }
    for id in &objects {
        deactivate(tx, id)?;
    }
    Ok(())
}

pub fn calculate(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "local.calc")?;
    let kind = text_of(input, "calculation_type")?;
    let inputs = refs_of(input, "confirmed_inputs")?;
    let assumption_ids = refs_of(input, "assumption_ids")?;
    let (records, assumptions, matter) = {
        let vault = ctx.lock()?;
        let mut records = vec![];
        let mut matters = std::collections::BTreeSet::new();
        for id in &inputs {
            let row: Option<(String, String, String, String, String, String, Option<String>, Option<String>, Option<String>, String)> = vault.db().query_row(
                "SELECT c.occurred_on,c.amount,c.currency,c.direction,c.kind,c.status,c.category,c.counterparty,c.account_id,c.matter_id FROM cashflows c JOIN objects o ON o.id=c.id WHERE c.id=? AND o.active=1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
            ).optional()?;
            let (date, amount, currency, direction, kind, status, category, counterparty, account, matter) = row.ok_or(Error::Denied)?;
            if status != "confirmed" {
                return Err(Error::NeedsInput("confirmed_inputs"));
            }
            matters.insert(matter);
            records.push(json!({"id": id, "date": date, "amount": amount, "currency": currency, "direction": direction, "kind": kind, "category": category, "counterparty": counterparty, "account": account}));
        }
        let mut assumptions = serde_json::Map::new();
        for id in &assumption_ids {
            let (key, value, matter): (String, String, String) = vault.db().query_row("SELECT a.kind,a.value,a.matter_id FROM assumptions a JOIN objects o ON o.id=a.id WHERE a.id=? AND o.active=1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.ok_or(Error::Denied)?;
            matters.insert(matter);
            assumptions.insert(key, serde_json::from_str(&value)?);
        }
        if matters.len() > 1 {
            return Err(Error::Denied);
        }
        (records, assumptions, matters.into_iter().next())
    };
    ctx.check()?;
    let request = json!({"op": "finance", "request": {"calculation_type": kind, "records": records, "assumptions": assumptions}});
    let computed = ctx.services.local.call(&request, ctx.cancel, Duration::from_secs(120))?;
    let payload = json!({"calculation_type": kind, "matter_id": matter, "inputs": inputs, "assumption_ids": assumption_ids, "assumptions": assumptions,
        "engine": computed["engine"], "assumptions_used": computed["assumptions_used"], "result": computed["result"], "computed_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "CalculationRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.processed = inputs;
    if let Some(status) = computed["result"]["irr"]["status"].as_str().filter(|s| *s != "unique") {
        parts.status = "partial".into();
        warn(&mut parts, &format!("irr_{status}"), "年化利率无法唯一确定（多重解、无解或不收敛），未输出单一年化数字。");
    }
    Ok(parts)
}
