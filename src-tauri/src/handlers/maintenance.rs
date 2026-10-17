//! vault-maintenance (encrypted backup, verify, restore to a new location) and
//! privacy-settings (grants and policy). Design 12.8: backups carry the key
//! wraps needed for recovery; restores never overwrite the current vault.
use super::*;
use crate::{
    crypto::SecretKey,
    vault::{self, VaultHeader, VaultKeys, now_rfc3339},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const BACKUP_FORMAT: &str = "lvjin.backup/1";

fn sql_literal(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or(Error::Contract)?;
    let text = text.strip_prefix("\\\\?\\").unwrap_or(text);
    Ok(format!("'{}'", text.replace('\'', "''")))
}

fn backup_db_key(keys: &VaultKeys, backup_id: &str) -> Result<SecretKey> {
    keys.backup.derive(backup_id.as_bytes(), b"lvjin/backup/db/v1")
}

fn manifest_key(keys: &VaultKeys, backup_id: &str) -> Result<SecretKey> {
    keys.backup.derive(backup_id.as_bytes(), b"lvjin/backup/manifest/v1")
}

fn file_sha256(path: &Path) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        digest.update(&buffer[..read]);
    }
    Ok((format!("{:x}", digest.finalize()), total))
}

/// Key a SQLCipher connection; the statement text lives only in a zeroizing buffer.
fn apply_key(db: &Connection, key: &SecretKey) -> Result<()> {
    let pragma = Zeroizing::new(format!("PRAGMA key=\"x'{}'\";", key.hex().as_str()));
    db.execute_batch(&pragma)?;
    Ok(())
}

/// Consistent encrypted copy of the live database via sqlcipher_export.
fn export_database(db: &Connection, target: &Path, key: &SecretKey) -> Result<()> {
    let attached = {
        let statement = Zeroizing::new(format!("ATTACH DATABASE {} AS backup KEY \"x'{}'\";", sql_literal(target)?, key.hex().as_str()));
        db.execute_batch(&statement)
    };
    attached?;
    let exported = db.query_row("SELECT sqlcipher_export('backup')", [], |_| Ok(()));
    let detached = db.execute_batch("DETACH DATABASE backup;");
    exported?;
    detached?;
    Ok(())
}

pub fn backup(vault: &crate::store::Vault, destination: &Path) -> Result<Value> {
    if !destination.is_dir() {
        return Err(Error::Worker("backup_destination_unavailable".into()));
    }
    let backup_id = format!("{:x}", uuid::Uuid::new_v4().simple());
    let name = format!("LvjinBackup-{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), &vault.id[..8]);
    let root = destination.join(&name);
    std::fs::create_dir(&root)?;
    let outcome = (|| -> Result<Value> {
        let db_path = root.join("vault.db");
        export_database(vault.db(), &db_path, &backup_db_key(&vault.keys, &backup_id)?)?;
        let (db_sha, db_bytes) = file_sha256(&db_path)?;
        let mut objects = vec![];
        for (_, source) in vault.blob_files()? {
            let shard = source.parent().and_then(|p| p.file_name()).ok_or(Error::Integrity)?.to_owned();
            let file_name = source.file_name().ok_or(Error::Integrity)?.to_owned();
            let target = root.join("objects").join(&shard).join(&file_name);
            std::fs::create_dir_all(target.parent().ok_or(Error::Integrity)?)?;
            std::fs::copy(&source, &target)?;
            let (source_sha, _) = file_sha256(&source)?;
            let (copied_sha, bytes) = file_sha256(&target)?;
            if source_sha != copied_sha {
                return Err(Error::Integrity);
            }
            objects.push(json!({"file": format!("objects/{}/{}", shard.to_string_lossy(), file_name.to_string_lossy()), "sha256": copied_sha, "bytes": bytes}));
        }
        let manifest = json!({"format": BACKUP_FORMAT, "backup_id": backup_id, "vault_id": vault.id, "created_at": now_rfc3339(), "schema_version": crate::store::SCHEMA_VERSION,
            "database": {"file": "vault.db", "sha256": db_sha, "bytes": db_bytes}, "objects": objects});
        let aad = format!("lvjin-backup/1|{}|{}", vault.id, backup_id);
        let sealed = manifest_key(&vault.keys, &backup_id)?.seal(serde_json::to_string(&manifest)?.as_bytes(), aad.as_bytes())?;
        let public = json!({"format": BACKUP_FORMAT, "backup_id": backup_id, "vault_id": vault.id, "created_at": manifest["created_at"],
            "header": vault::backup_header(&vault.header), "manifest": STANDARD.encode(sealed),
            "notice": "加密备份：需要原口令或恢复码才能恢复；API 密钥与外部连接不在备份内。"});
        std::fs::write(root.join("backup.json"), serde_json::to_vec_pretty(&public)?)?;
        Ok(json!({"backup_name": name, "backup_id": backup_id, "objects": manifest["objects"].as_array().map(Vec::len), "database_bytes": db_bytes}))
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&root);
    }
    outcome
}

pub struct BackupPackage {
    pub root: PathBuf,
    pub public: Value,
    pub header: VaultHeader,
}

pub fn read_backup(root: &Path) -> Result<BackupPackage> {
    let public: Value = serde_json::from_slice(&std::fs::read(root.join("backup.json")).map_err(|_| Error::Worker("not_a_backup".into()))?)?;
    if public["format"] != json!(BACKUP_FORMAT) {
        return Err(Error::Worker("unsupported_backup_format".into()));
    }
    let header: VaultHeader = serde_json::from_value(public["header"].clone())?;
    if public["vault_id"].as_str() != Some(header.vault_id.as_str()) {
        return Err(Error::Integrity);
    }
    Ok(BackupPackage { root: root.to_path_buf(), public, header })
}

/// Authenticate the manifest and every file hash; open the database copy.
pub fn verify_package(package: &BackupPackage, keys: &VaultKeys) -> Result<Value> {
    let backup_id = package.public["backup_id"].as_str().ok_or(Error::Integrity)?;
    let aad = format!("lvjin-backup/1|{}|{}", package.header.vault_id, backup_id);
    let sealed = STANDARD.decode(package.public["manifest"].as_str().ok_or(Error::Integrity)?).map_err(|_| Error::Integrity)?;
    let manifest: Value = serde_json::from_slice(&manifest_key(keys, backup_id)?.open(&sealed, aad.as_bytes()).map_err(|_| Error::Worker("backup_authentication_failed".into()))?)?;
    let mut problems = vec![];
    let mut checked = 0;
    let mut entries = vec![manifest["database"].clone()];
    entries.extend(manifest["objects"].as_array().cloned().unwrap_or_default());
    for entry in &entries {
        let relative = entry["file"].as_str().ok_or(Error::Integrity)?;
        if relative.contains("..") || relative.starts_with('/') || relative.contains(':') {
            return Err(Error::Integrity);
        }
        match file_sha256(&package.root.join(relative)) {
            Ok((digest, bytes)) if Some(digest.as_str()) == entry["sha256"].as_str() && Some(bytes) == entry["bytes"].as_u64() => checked += 1,
            _ => problems.push(json!({"file": relative, "problem": "missing_or_modified"})),
        }
    }
    // A modified package is reported, never parsed: only hash-verified bytes
    // reach SQLCipher.
    let mut versions = -1i64;
    if problems.is_empty() {
        let db = Connection::open(package.root.join("vault.db"))?;
        apply_key(&db, &backup_db_key(keys, backup_id)?)?;
        let integrity: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(|_| Error::Worker("backup_database_unreadable".into()))?;
        if integrity != "ok" {
            problems.push(json!({"file": "vault.db", "problem": "integrity_check_failed"}));
        }
        versions = db.query_row("SELECT COUNT(*) FROM document_versions", [], |r| r.get(0)).unwrap_or(-1);
    }
    Ok(json!({"ok": problems.is_empty(), "checked_files": checked, "problems": problems, "document_versions": versions, "created_at": manifest["created_at"], "vault_id": manifest["vault_id"]}))
}

/// Restore into a new directory under `vaults_root`. Never touches an
/// existing vault directory; returns the new vault handle.
pub fn restore(package: &BackupPackage, keys: &VaultKeys, vaults_root: &Path) -> Result<String> {
    let report = verify_package(package, keys)?;
    if report["ok"] != json!(true) {
        return Err(Error::Worker("backup_verification_failed".into()));
    }
    let backup_id = package.public["backup_id"].as_str().ok_or(Error::Integrity)?;
    let handle = format!("{}-r{}", package.header.vault_id, chrono::Local::now().format("%Y%m%d%H%M%S"));
    let target = vault::directory_for(vaults_root, &handle)?;
    if target.exists() {
        return Err(Error::Conflict);
    }
    std::fs::create_dir_all(target.join(vault::OBJECTS_DIR))?;
    let outcome = (|| -> Result<()> {
        let sealed = STANDARD.decode(package.public["manifest"].as_str().ok_or(Error::Integrity)?).map_err(|_| Error::Integrity)?;
        let aad = format!("lvjin-backup/1|{}|{}", package.header.vault_id, backup_id);
        let manifest: Value = serde_json::from_slice(&manifest_key(keys, backup_id)?.open(&sealed, aad.as_bytes())?)?;
        for entry in manifest["objects"].as_array().cloned().unwrap_or_default() {
            let relative = entry["file"].as_str().ok_or(Error::Integrity)?;
            let destination = target.join(relative);
            std::fs::create_dir_all(destination.parent().ok_or(Error::Integrity)?)?;
            std::fs::copy(package.root.join(relative), &destination)?;
        }
        let source = Connection::open(package.root.join("vault.db"))?;
        apply_key(&source, &backup_db_key(keys, backup_id)?)?;
        export_database(&source, &target.join(vault::DB_FILE), &keys.db)?;
        drop(source);
        let restored = Connection::open(target.join(vault::DB_FILE))?;
        apply_key(&restored, &keys.db)?;
        // sqlcipher_export does not copy user_version; record the schema level.
        restored.execute_batch(&format!("PRAGMA user_version={};", crate::store::SCHEMA_VERSION))?;
        drop(restored);
        vault::write_header(&target, &package.header)?;
        Ok(())
    })();
    if let Err(error) = outcome {
        let _ = std::fs::remove_dir_all(&target);
        return Err(error);
    }
    Ok(handle)
}

fn selection_path(ctx: &Ctx, id: &str, purpose: &str) -> Result<PathBuf> {
    let path: String = ctx
        .lock()?
        .db()
        .query_row("SELECT path FROM selections WHERE id=? AND kind='MaintenanceSelectionRef' AND purpose=?", params![id, purpose], |r| r.get(0))
        .optional()?
        .ok_or(Error::Denied)?;
    Ok(PathBuf::from(path))
}

pub fn maintain(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.maintenance")?;
    ctx.authorize(invocation, "file.backup")?;
    let operation = text_of(input, "operation")?;
    let selection = text_of(input, "selection_id")?;
    {
        let vault = ctx.lock()?;
        let action = vault.runtime.root_action(invocation.root_id())?;
        if !matches!(action.as_str(), "confirm" | "protected" | "system") {
            return Err(Error::Denied);
        }
    }
    let payload = match operation.as_str() {
        "backup" => {
            let destination = selection_path(ctx, &selection, "backup_destination")?;
            ctx.report(invocation, "backup", 0, 1);
            let vault = ctx.lock()?;
            let mut summary = backup(&vault, &destination)?;
            summary["operation"] = json!("backup");
            summary
        }
        "verify" => {
            let source = selection_path(ctx, &selection, "backup_source")?;
            let package = read_backup(&source)?;
            let vault = ctx.lock()?;
            if package.header.vault_id != vault.id {
                return Err(Error::NeedsInput("backup_passphrase"));
            }
            let mut report = verify_package(&package, &vault.keys)?;
            report["operation"] = json!("verify");
            report
        }
        "restore" => {
            let source = selection_path(ctx, &selection, "backup_source")?;
            let package = read_backup(&source)?;
            let vault = ctx.lock()?;
            if package.header.vault_id != vault.id {
                return Err(Error::NeedsInput("backup_passphrase"));
            }
            let handle = restore(&package, &vault.keys, &ctx.services.layout.vaults_root())?;
            json!({"operation": "restore", "restored_handle": handle, "note": "已恢复到新的空间目录，当前空间未被覆盖。"})
        }
        _ => return Err(Error::Contract),
    };
    let vault = ctx.lock()?;
    vault.db().execute("INSERT INTO audit(root,invocation,code) VALUES(?,?,?)", params![invocation.root_id(), invocation.id(), format!("maintenance.{operation}")])?;
    let result = vault.put_result(invocation.skill(), invocation.id(), "MaintenanceResultRef", &payload)?;
    let mut parts = completed(Some(result));
    if payload["ok"] == json!(false) {
        parts.status = "failed".into();
        warn(&mut parts, "backup_problems", "备份中有文件缺失或被修改。");
    }
    Ok(parts)
}

pub fn privacy(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "settings.privacy")?;
    let operation = text_of(input, "operation")?;
    let request = text_of(input, "ui_request_id")?;
    let vault = ctx.lock()?;
    let (recorded, payload, consumed): (String, String, i64) = vault
        .db()
        .query_row("SELECT operation,payload,consumed FROM ui_requests WHERE id=?", [&request], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?
        .ok_or(Error::Denied)?;
    if recorded != operation || consumed != 0 {
        return Err(Error::Denied);
    }
    let payload: Value = serde_json::from_str(&payload)?;
    let output = match operation.as_str() {
        "create" | "unlock" | "lock" => json!({"operation": operation, "recorded": true}),
        "view_grants" => {
            let mut statement = vault.db().prepare("SELECT r.id,r.action,r.cancelled,r.created_at,(SELECT group_concat(capability,',') FROM grants g WHERE g.root=r.id) FROM roots r ORDER BY r.created_at DESC LIMIT 100")?;
            let rows = statement.query_map([], |r| Ok(json!({"root": r.get::<_, String>(0)?, "action": r.get::<_, String>(1)?, "cancelled": r.get::<_, i64>(2)? != 0, "created_at": r.get::<_, Option<String>>(3)?, "capabilities": r.get::<_, Option<String>>(4)?})))?;
            json!({"operation": operation, "roots": rows.collect::<std::result::Result<Vec<_>, _>>()?, "network_grants": 0})
        }
        "revoke_grant" => {
            let root = payload["root"].as_str().ok_or(Error::Contract)?;
            let capability = payload["capability"].as_str().ok_or(Error::Contract)?;
            vault.db().execute("DELETE FROM grants WHERE root=? AND capability=?", params![root, capability])?;
            vault.db().execute("UPDATE roots SET revision=revision+1 WHERE id=?", [root])?;
            json!({"operation": operation, "root": root, "capability": capability})
        }
        "change_policy" => {
            if let Some(minutes) = payload["auto_lock_minutes"].as_u64() {
                if !(1..=240).contains(&minutes) {
                    return Err(Error::Contract);
                }
                vault.set_setting("auto_lock_minutes", &minutes.to_string())?;
            }
            json!({"operation": operation, "applied": payload.as_object().map(|m| m.keys().cloned().collect::<Vec<_>>())})
        }
        _ => return Err(Error::Contract),
    };
    vault.db().execute("UPDATE ui_requests SET consumed=1 WHERE id=?", [&request])?;
    vault.db().execute("INSERT INTO audit(root,invocation,code) VALUES(?,?,?)", params![invocation.root_id(), invocation.id(), format!("privacy.{operation}")])?;
    let result = vault.put_result(invocation.skill(), invocation.id(), "ResultRef", &output)?;
    Ok(completed(Some(result)))
}
