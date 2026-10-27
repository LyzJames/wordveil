//! document-import, document-ocr and evidence-read (design 9.5).
use super::*;
use crate::{
    geometry::{BackwardGrid, Point, unrotate},
    store::sha256_hex,
    vault::now_rfc3339,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, time::Duration};

const MAX_DOCUMENT_BYTES: u64 = 200 * 1024 * 1024;
const MAX_MEDIA_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_CANDIDATES: usize = 300;
const IPC_BATCH_CHARS: usize = 10 * 1024 * 1024;

/// Container detection for media routed to media-decode (documents go to the
/// isolated parser's own sniffing). Extensions are never trusted.
pub fn media_kind_by_magic(head: &[u8]) -> Option<&'static str> {
    if head.len() >= 12 && &head[..4] == b"RIFF" {
        return match &head[8..12] {
            b"WAVE" => Some("audio"),
            b"AVI " => Some("video"),
            _ => None,
        };
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return if matches!(&head[8..12], b"V0_5_0A " | b"V0_5_0B " | b"F4A ") { Some("audio") } else { Some("video") };
    }
    if head.starts_with(b"ID3") || head.starts_with(b"OggS") || head.starts_with(b"fLaC") || head.starts_with(b"#!AMR") {
        return Some("audio");
    }
    if head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xE0) == 0xE0 && head[1] != 0xFF {
        return Some("audio");
    }
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) || head.starts_with(b"FLV") {
        return Some("video");
    }
    None
}

fn read_head(path: &std::path::Path) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut head = vec![0u8; 64];
    let read = file.read(&mut head)?;
    head.truncate(read);
    Ok(head)
}

struct Selection {
    path: std::path::PathBuf,
    display: String,
    bytes: i64,
    sha256: String,
    consumed: bool,
    artifact: Option<zeroize::Zeroizing<Vec<u8>>>,
}

fn load_selection(ctx: &Ctx, id: &str, purpose: &str, root: &str, target: &str) -> Result<Selection> {
    let vault = ctx.lock()?;
    if matches!(vault.object_kind(id)?.as_str(), "OfflinePackageArtifactRef" | "DownloadedArtifactRef") {
        vault.expect_kind(target, &["PublicKnowledgeCollectionRef"])?;
        let (display, blob, bytes, hash, consumed):(String,String,i64,String,i64)=vault.db().query_row(
            "SELECT display_name,blob,bytes,sha256,consumed FROM trusted_artifacts WHERE id=? AND root=? AND target=?",
            params![id,root,target],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?.ok_or(Error::Denied)?;
        let data=vault.get_blob(&blob)?;
        if data.len() as i64!=bytes || sha256_hex(&data)!=hash {return Err(Error::Integrity);}
        return Ok(Selection{path:std::path::PathBuf::new(),display,bytes,sha256:hash,consumed:consumed!=0,artifact:Some(data)});
    }
    vault.expect_kind(target, &["MatterRef"])?;
    let row: Option<(String, String, Option<i64>, Option<String>, String, i64)> = vault
        .db()
        .query_row("SELECT path,display_name,bytes,sha256,purpose,consumed FROM selections WHERE id=? AND kind='SelectionRef'", [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .optional()?;
    let (path, display, bytes, sha256, recorded_purpose, consumed) = row.ok_or(Error::Denied)?;
    if recorded_purpose != purpose {
        return Err(Error::Denied);
    }
    Ok(Selection { path: path.into(), display, bytes: bytes.unwrap_or(-1), sha256: sha256.unwrap_or_default(), consumed: consumed != 0, artifact: None })
}

fn hash_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn extension(display: &str) -> String {
    display.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default()
}

pub fn import(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    let selection_id = text_of(input, "selection_id")?;
    let target = text_of(input, "target_ref")?;
    let mode = text_of(input, "import_mode")?;
    ctx.authorize(invocation, "file.selection.read")?;
    ctx.authorize(invocation, "vault.write")?;
    {
        let vault = ctx.lock()?;
        vault.expect_kind(&target, &["MatterRef", "PublicKnowledgeCollectionRef"])?;
        // The root is the general app's personal file collection. It has the
        // same vault scope and immutable import-selection checks as a project;
        // the retired specialist UI required creating a matter first.
        // Idempotent replay: a completed import of this selection returns its result.
        if let Some(outcome) = vault.db().query_row("SELECT outcome FROM commit_log WHERE idempotency_key=?", [format!("import:{selection_id}")], |r| r.get::<_, String>(0)).optional()? {
            let outcome: Value = serde_json::from_str(&outcome)?;
            let mut parts = completed(outcome["result_ref"].as_str().map(str::to_owned));
            warn(&mut parts, "already_imported", "该选择凭证已导入完成，返回原结果，未重复导入。");
            return Ok(parts);
        }
    }
    let selection = load_selection(ctx, &selection_id, "import", invocation.root_id(), &target)?;
    // Resume an interrupted import of this very selection instead of duplicating it.
    let interrupted: Option<String> = {
        let vault = ctx.lock()?;
        vault
            .db()
            .query_row("SELECT id FROM jobs WHERE kind='import' AND status IN ('running','interrupted') AND json_extract(state,'$.selection')=?", [&selection_id], |r| r.get(0))
            .optional()?
    };
    if selection.consumed && interrupted.is_none() {
        return Err(Error::Conflict);
    }
    let version_id = match interrupted {
        Some(version) => version,
        None => store_original(ctx, invocation, &selection_id, &selection, &target, &mode)?,
    };
    if let Some(existing) = version_id.strip_prefix("duplicate:") {
        let result = {
            let vault = ctx.lock()?;
            let payload = json!({"version_id": existing, "duplicate_of": existing, "status": "duplicate"});
            let result = vault.put_result(invocation.skill(), invocation.id(), "ImportResultRef", &payload)?;
            vault.db().execute("UPDATE selections SET consumed=1 WHERE id=?", [&selection_id])?;
            vault.db().execute("UPDATE trusted_artifacts SET consumed=1 WHERE id=?", [&selection_id])?;
            vault.commit_once(&format!("import:{selection_id}"), invocation.root_id(), "import", Some(existing), &json!({"result_ref": result}))?;
            result
        };
        let mut parts = completed(Some(result));
        parts.evidence_refs.push(existing.to_owned());
        warn(&mut parts, "duplicate_existing_version", "该事务中已有内容完全相同的资料，未重复导入，已指向原有版本。");
        return Ok(parts);
    }
    match process_version(ctx, invocation, &version_id) {
        Ok((payload, mut parts)) => {
            let vault = ctx.lock()?;
            let result = vault.put_result(invocation.skill(), invocation.id(), "ImportResultRef", &payload)?;
            vault.db().execute("UPDATE jobs SET status='done',updated_at=? WHERE id=?", params![now_rfc3339(), version_id])?;
            vault.db().execute("UPDATE selections SET consumed=1 WHERE id=?", [&selection_id])?;
            vault.db().execute("UPDATE trusted_artifacts SET consumed=1 WHERE id=?", [&selection_id])?;
            vault.commit_once(&format!("import:{selection_id}"), invocation.root_id(), "import", Some(&version_id), &json!({"result_ref": result}))?;
            parts.data_ref = Some(result);
            parts.evidence_refs.insert(0, version_id);
            Ok(parts)
        }
        Err(Error::Cancelled) => {
            // Kept encrypted for resumption from the same selection.
            let vault = ctx.lock()?;
            vault.db().execute("UPDATE jobs SET status='interrupted',updated_at=? WHERE id=?", params![now_rfc3339(), version_id])?;
            vault.db().execute("UPDATE document_versions SET status='interrupted',error_code='cancelled' WHERE id=?", [&version_id])?;
            Err(Error::Cancelled)
        }
        Err(error) => {
            // A failed import keeps nothing: the encrypted original and every
            // partial derivative are removed; the selection stays reusable.
            discard_version(ctx, &version_id)?;
            Err(error)
        }
    }
}

/// Remove a version whose import failed, with all derived rows and blobs.
fn discard_version(ctx: &Ctx, version: &str) -> Result<()> {
    let mut vault = ctx.lock()?;
    let indexed: i64 = vault.db().query_row("SELECT COUNT(*) FROM chunks WHERE version_id=?", [version], |r| r.get(0))?;
    let mut removed = vec![];
    let tx = vault.runtime.ledger.db.transaction()?;
    super::matters::delete_version(&tx, version, &mut removed)?;
    tx.commit()?;
    for blob in removed {
        vault.delete_blob(&blob)?;
    }
    if indexed > 0 {
        vault.db().execute("UPDATE index_state SET dirty=1 WHERE id=1", [])?;
        vault.dense = None;
    }
    Ok(())
}

/// Verify the selected file again (TOCTOU), deduplicate inside the matter and
/// store the encrypted original. Returns the new version id or `duplicate:<id>`.
fn store_original(ctx: &Ctx, invocation: &Invocation, selection_id: &str, selection: &Selection, matter: &str, mode: &str) -> Result<String> {
    let bytes = if let Some(data)=&selection.artifact { data.len() as u64 } else {
        let metadata = std::fs::metadata(&selection.path).map_err(|_| Error::Worker("selection_unavailable".into()))?;
        if !metadata.is_file() { return Err(Error::Worker("selection_changed".into())); }
        metadata.len()
    };
    if bytes as i64 != selection.bytes {
        return Err(Error::Worker("selection_changed".into()));
    }
    let head = match &selection.artifact {Some(data)=>data.iter().take(64).copied().collect(),None=>read_head(&selection.path)?};
    let media = media_kind_by_magic(&head);
    let limit = if media.is_some() { MAX_MEDIA_BYTES } else { MAX_DOCUMENT_BYTES };
    if bytes > limit {
        return Err(Error::Worker("file_too_large".into()));
    }
    let digest = match &selection.artifact {Some(data)=>sha256_hex(data),None=>hash_file(&selection.path)?};
    if digest != selection.sha256 {
        return Err(Error::Worker("selection_changed".into()));
    }
    let vault = ctx.lock()?;
    let duplicate: Option<String> = vault
        .db()
        .query_row(
            "SELECT v.id FROM document_versions v JOIN documents d ON d.id=v.document_id JOIN objects o ON o.id=v.id WHERE d.matter_id=? AND v.content_sha256=? AND o.active=1 LIMIT 1",
            params![matter, digest],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(existing) = duplicate {
        if mode == "copy" {
            return Ok(format!("duplicate:{existing}"));
        }
    }
    let previous: Option<(String, i64)> = if mode == "new_version" {
        vault
            .db()
            .query_row(
                "SELECT d.id, MAX(v.version_no) FROM documents d JOIN document_versions v ON v.document_id=d.id JOIN objects o ON o.id=v.id WHERE d.matter_id=? AND d.display_name=? AND o.active=1 GROUP BY d.id",
                params![matter, selection.display],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
    } else {
        None
    };
    let version_id = vault.register("DocumentVersionRef", "0.2.0", Some(matter))?;
    let blob = if let Some(data)=&selection.artifact { vault.put_blob(&version_id,"original",data)? } else {
        let file = std::io::BufReader::new(std::fs::File::open(&selection.path)?);
        vault.put_blob_reader(&version_id, "original", file, bytes)?
    };
    if vault.blob_sha256(&blob)? != digest {
        vault.delete_blob(&blob)?;
        return Err(Error::Worker("selection_changed".into()));
    }
    let now = now_rfc3339();
    let (document_id, version_no) = match previous {
        Some((document, number)) => (document, number + 1),
        None => {
            let document = format!("doc_{}", uuid::Uuid::new_v4().simple());
            vault.db().execute("INSERT INTO documents(id,matter_id,display_name,created_at) VALUES(?,?,?,?)", params![document, matter, selection.display, now])?;
            (document, 1)
        }
    };
    let kind = media.unwrap_or("document");
    vault.db().execute(
        "INSERT INTO document_versions(id,document_id,version_no,content_sha256,bytes,media_kind,original_blob,status,created_at) VALUES(?,?,?,?,?,?,?,'stored',?)",
        params![version_id, document_id, version_no, digest, bytes as i64, kind, blob, now],
    )?;
    vault.db().execute(
        "INSERT INTO jobs(id,root,kind,state,status,updated_at) VALUES(?,?,'import',?,'running',?)",
        params![version_id, invocation.root_id(), json!({"selection": selection_id, "step": "stored"}).to_string(), now],
    )?;
    Ok(version_id)
}

struct BlockRow {
    page_id: Option<String>,
    kind: &'static str,
    text: String,
    locator: Value,
    confidence: Option<f64>,
    source: &'static str,
}

fn insert_blocks(ctx: &Ctx, version: &str, rows: &[BlockRow], replace_pages: Option<&[String]>) -> Result<usize> {
    let mut vault = ctx.lock()?;
    let tx = vault.runtime.ledger.db.transaction()?;
    match replace_pages {
        Some(pages) => {
            for page in pages {
                tx.execute("DELETE FROM blocks WHERE version_id=? AND page_id=? AND source='ocr'", params![version, page])?;
            }
        }
        None => {
            tx.execute("DELETE FROM blocks WHERE version_id=? AND source<>'ocr' AND source<>'asr'", [version])?;
        }
    }
    let base: i64 = tx.query_row("SELECT COALESCE(MAX(ordinal),-1)+1 FROM blocks WHERE version_id=?", [version], |r| r.get(0))?;
    for (index, row) in rows.iter().enumerate() {
        tx.execute(
            "INSERT INTO blocks(version_id,page_id,ordinal,kind,text,locator,confidence,source) VALUES(?,?,?,?,?,?,?,?)",
            params![version, row.page_id, base + index as i64, row.kind, row.text, row.locator.to_string(), row.confidence, row.source],
        )?;
    }
    tx.commit()?;
    Ok(rows.len())
}

fn insert_page(ctx: &Ctx, version: &str, page_no: i64, source: &str, width: f64, height: f64, image: Option<&[u8]>) -> Result<String> {
    let vault = ctx.lock()?;
    if let Some(existing) = vault.db().query_row("SELECT id FROM pages WHERE version_id=? AND page_no=?", params![version, page_no], |r| r.get::<_, String>(0)).optional()? {
        return Ok(existing);
    }
    let page = vault.register("PageRef", "0.2.0", Some(version))?;
    let blob = match image {
        Some(bytes) => Some(vault.put_blob(&page, "page_image", bytes)?),
        None => None,
    };
    vault.db().execute(
        "INSERT INTO pages(id,version_id,page_no,source,width,height,image_blob,created_at) VALUES(?,?,?,?,?,?,?,?)",
        params![page, version, page_no, source, width, height, blob, now_rfc3339()],
    )?;
    Ok(page)
}

fn local(ctx: &Ctx, request: Value, seconds: u64) -> Result<Value> {
    ctx.check()?;
    ctx.services.local.call(&request, ctx.cancel, Duration::from_secs(seconds))
}

fn png_bytes(value: &Value) -> Result<Vec<u8>> {
    STANDARD.decode(value["png_base64"].as_str().ok_or(Error::Contract)?).map_err(|_| Error::Contract)
}

/// Parse a stored version into pages/blocks, OCR scanned pages, extract
/// candidate fields and build the index. Re-running is idempotent.
fn process_version(ctx: &Ctx, invocation: &Invocation, version: &str) -> Result<(Value, EnvelopeParts)> {
    let (kind, display, blob, matter, version_no, bytes, digest): (String, String, String, String, i64, i64, String) = {
        let vault = ctx.lock()?;
        vault.db().query_row(
            "SELECT v.media_kind,d.display_name,v.original_blob,d.matter_id,v.version_no,v.bytes,v.content_sha256 FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?",
            [version],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )?
    };
    let mut parts = completed(None);
    let checkpoint:Option<String>=ctx.lock()?.db().query_row("SELECT json_extract(state,'$.parse_summary') FROM jobs WHERE id=? AND json_extract(state,'$.phase')='indexing'",[version],|r|r.get(0)).optional()?.flatten();
    if let Some(summary)=checkpoint {
        let summary:Value=serde_json::from_str(&summary)?;
        let (_,index)=child(ctx,invocation,"knowledge-index",json!({"document_versions":[version],"operation":"build"}))?;
        ctx.check()?;
        let indexed=status_of(&index)=="completed";
        if !indexed {parts.warnings.extend(child_warnings(&index));parts.status="partial".into();}
        let ocr_ok=summary["ocr_ok"]==json!(true);
        let status=if indexed&&ocr_ok{"ready"}else{"partial"};
        if status=="partial"{parts.status="partial".into();}
        let vault=ctx.lock()?;
        vault.db().execute("UPDATE document_versions SET status=?,error_code=NULL WHERE id=?",params![status,version])?;
        let chunks:i64=vault.db().query_row("SELECT COUNT(*) FROM chunks WHERE version_id=?",[version],|r|r.get(0))?;
        let blocks:i64=vault.db().query_row("SELECT COUNT(*) FROM blocks WHERE version_id=?",[version],|r|r.get(0))?;
        let payload=json!({"version_id":version,"version_no":version_no,"display_name":display,"media_kind":kind,"format":summary["format"],"bytes":bytes,"content_sha256":digest,"status":status,"pages":summary["pages"],"ocr_pages":summary["ocr_pages"],"native_blocks":summary["native_blocks"],"blocks":blocks,"chunks":chunks,"candidate_facts":summary["candidate_facts"],"resumed_from":"index_checkpoint"});
        return Ok((payload,parts));
    }
    if kind == "audio" || kind == "video" {
        let media = {
            let vault = ctx.lock()?;
            let existing = vault.db().query_row("SELECT id FROM media WHERE version_id=?", [version], |r| r.get::<_, String>(0)).optional()?;
            match existing {
                Some(id) => id,
                None => {
                    let media = vault.register("MediaVersionRef", "0.2.0", Some(version))?;
                    vault.db().execute(
                        "INSERT INTO media(id,version_id,probe,duration_s,has_audio,has_video,created_at) VALUES(?,?,'{}',NULL,?,?,?)",
                        params![media, version, i64::from(kind == "audio" || kind == "video"), i64::from(kind == "video"), now_rfc3339()],
                    )?;
                    media
                }
            }
        };
        ctx.lock()?.db().execute("UPDATE document_versions SET status='stored_media' WHERE id=?", [version])?;
        warn(&mut parts, "transcription_separate_step", "音视频原件已加密保存；转写与抽帧识别需单独运行（不会自动外发）。");
        let payload = json!({"version_id": version, "version_no": version_no, "display_name": display, "media_kind": kind, "media_id": media,
            "bytes": bytes, "content_sha256": digest, "status": "stored_media", "pages": 0, "blocks": 0, "chunks": 0, "candidate_facts": 0});
        parts.evidence_refs.push(media);
        return Ok((payload, parts));
    }
    ctx.lock()?.db().execute("UPDATE document_versions SET status='parsing' WHERE id=?", [version])?;
    let data = ctx.lock()?.get_blob(&blob)?;
    let upload = ctx.services.local.upload(&data, ctx.cancel)?;
    let sniffed = local(ctx, json!({"op": "sniff", "upload_id": upload.id}), 60)?;
    let format = sniffed["kind"].as_str().unwrap_or("unknown").to_owned();
    let mut rows: Vec<BlockRow> = vec![];
    let mut ocr_pages: Vec<String> = vec![];
    let mut page_count = 0i64;
    match format.as_str() {
        "pdf" => {
            let info = local(ctx, json!({"op": "pdf_info", "upload_id": upload.id}), 120)?;
            let total = info["page_count"].as_i64().ok_or(Error::Contract)?;
            page_count = total;
            let mut first = 1;
            while first <= total {
                ctx.check()?;
                let last = (first + 19).min(total);
                ctx.report(invocation, "parse_pdf", first as u64, total as u64);
                let batch = local(ctx, json!({"op": "pdf_pages", "upload_id": upload.id, "first": first, "last": last}), 300)?;
                for page in batch["pages"].as_array().ok_or(Error::Contract)? {
                    let page_no = page["page_no"].as_i64().ok_or(Error::Contract)?;
                    let page_id = if page["needs_ocr"].as_bool() == Some(true) {
                        let rendered = local(ctx, json!({"op": "pdf_render", "upload_id": upload.id, "first": page_no, "purpose": "ocr"}), 300)?;
                        let image = png_bytes(&rendered)?;
                        let page_id = insert_page(ctx, version, page_no, "scan", rendered["width"].as_f64().unwrap_or(0.0), rendered["height"].as_f64().unwrap_or(0.0), Some(&image))?;
                        ocr_pages.push(page_id.clone());
                        page_id
                    } else {
                        insert_page(ctx, version, page_no, "native", page["display_width_pt"].as_f64().unwrap_or(0.0), page["display_height_pt"].as_f64().unwrap_or(0.0), None)?
                    };
                    for line in page["lines"].as_array().ok_or(Error::Contract)? {
                        let mut locator = json!({"type": "page", "page_no": page_no, "page_id": page_id, "bbox": line["bbox"], "frame": "page"});
                        if line["outside_page"].as_bool() == Some(true) {
                            locator["outside_page"] = json!(true);
                        }
                        rows.push(BlockRow { page_id: Some(page_id.clone()), kind: "text_line", text: line["text"].as_str().unwrap_or("").to_owned(), locator, confidence: None, source: "native" });
                    }
                }
                first = last + 1;
            }
        }
        "image" => {
            let pages = local(ctx, json!({"op": "image_pages", "upload_id": upload.id}), 300)?;
            for page in pages["pages"].as_array().ok_or(Error::Contract)? {
                let image = png_bytes(page)?;
                let page_no = page["page_no"].as_i64().unwrap_or(1);
                let page_id = insert_page(ctx, version, page_no, "image", page["width"].as_f64().unwrap_or(0.0), page["height"].as_f64().unwrap_or(0.0), Some(&image))?;
                ocr_pages.push(page_id);
                page_count += 1;
            }
        }
        "docx" => {
            let parsed = local(ctx, json!({"op": "docx_blocks", "upload_id": upload.id}), 300)?;
            let mut table_rows: BTreeMap<(i64, i64), Vec<(i64, String)>> = BTreeMap::new();
            for block in parsed["blocks"].as_array().ok_or(Error::Contract)? {
                let text = block["text"].as_str().unwrap_or("").to_owned();
                match block["kind"].as_str() {
                    Some("table_cell") => {
                        let key = (block["locator"]["table"].as_i64().unwrap_or(0), block["locator"]["row"].as_i64().unwrap_or(0));
                        table_rows.entry(key).or_default().push((block["locator"]["col"].as_i64().unwrap_or(0), text));
                    }
                    Some(kind) => {
                        flush_tables(&mut table_rows, &mut rows);
                        let mut locator = block["locator"].clone();
                        locator["type"] = json!("docx");
                        if let Some(level) = block["level"].as_i64() {
                            locator["heading_level"] = json!(level);
                        }
                        rows.push(BlockRow { page_id: None, kind: if kind == "heading" { "heading" } else { "paragraph" }, text, locator, confidence: None, source: "native" });
                    }
                    None => {}
                }
            }
            flush_tables(&mut table_rows, &mut rows);
        }
        "xlsx" => {
            let info = local(ctx, json!({"op": "xlsx_info", "upload_id": upload.id}), 120)?;
            let mut unconfirmed = 0;
            for sheet in info["sheets"].as_array().ok_or(Error::Contract)? {
                let name = sheet["name"].as_str().unwrap_or("").to_owned();
                let total = sheet["max_row"].as_i64().unwrap_or(0);
                let mut collected: Vec<(i64, Vec<Value>)> = vec![];
                let mut offset = 0;
                while offset < total {
                    ctx.check()?;
                    let chunk = local(ctx, json!({"op": "xlsx_rows", "upload_id": upload.id, "sheet": name, "offset": offset, "limit": 2000}), 300)?;
                    for row in chunk["rows"].as_array().ok_or(Error::Contract)? {
                        let cells = row["cells"].as_array().cloned().unwrap_or_default();
                        unconfirmed += cells.iter().filter(|c| c["unconfirmed"] == json!(true)).count();
                        let values: Vec<Value> = cells.iter().map(|c| if c.is_null() { json!({"value": null}) } else { c.clone() }).collect();
                        collected.push((row["row"].as_i64().unwrap_or(0), values));
                    }
                    offset += 2000;
                }
                table_blocks(&mut rows, Some(&name), collected);
            }
            if unconfirmed > 0 {
                warn(&mut parts, "formula_value_unconfirmed", "表格中有公式单元格没有保存计算结果；本应用不执行公式，这些单元格未采用，请在原表格中核对。");
            }
        }
        "text" => {
            let ext = extension(&display);
            if matches!(ext.as_str(), "csv" | "tsv") {
                let mut collected: Vec<(i64, Vec<Value>)> = vec![];
                let mut offset = 0;
                loop {
                    ctx.check()?;
                    let chunk = local(ctx, json!({"op": "delimited_rows", "upload_id": upload.id, "offset": offset, "limit": 2000}), 300)?;
                    let batch = chunk["rows"].as_array().cloned().unwrap_or_default();
                    for row in &batch {
                        let values: Vec<Value> = row["cells"].as_array().cloned().unwrap_or_default().iter().enumerate()
                            .map(|(index, cell)| json!({"col": index + 1, "value": cell.as_str().filter(|s| !s.is_empty())}))
                            .collect();
                        collected.push((row["row"].as_i64().unwrap_or(0), values));
                    }
                    if batch.len() < 2000 {
                        break;
                    }
                    offset += 2000;
                }
                table_blocks(&mut rows, None, collected);
            } else {
                let parsed = local(ctx, json!({"op": "text_blocks", "upload_id": upload.id}), 120)?;
                for block in parsed["blocks"].as_array().ok_or(Error::Contract)? {
                    let mut locator = block["locator"].clone();
                    locator["type"] = json!("text");
                    rows.push(BlockRow { page_id: None, kind: "text_line", text: block["text"].as_str().unwrap_or("").to_owned(), locator, confidence: None, source: "native" });
                }
            }
        }
        "ole" => return Err(Error::Worker("legacy_office_not_supported".into())),
        _ => return Err(Error::Worker("unsupported_format".into())),
    }
    drop(upload);
    let written = insert_blocks(ctx, version, &rows, None)?;
    ctx.lock()?.db().execute("UPDATE document_versions SET page_count=?,status='ocr' WHERE id=?", params![page_count, version])?;
    let mut ocr_ok = true;
    for page_batch in ocr_pages.chunks(200) {
        let (_, envelope) = child(ctx, invocation, "document-ocr", json!({"document_version": version, "page_ids": page_batch, "purpose": "index"}))?;
        ctx.check()?;
        if status_of(&envelope) != "completed" {
            ocr_ok = false;
            parts.warnings.extend(child_warnings(&envelope));
        }
    }
    // General file ingestion never extracts legal/financial business facts.
    // Existing historical facts remain readable in the encrypted vault.
    let candidates = 0usize;
    let summary=json!({"format":format,"pages":page_count,"ocr_pages":ocr_pages.len(),"native_blocks":written,"candidate_facts":candidates,"ocr_ok":ocr_ok});
    ctx.lock()?.db().execute("UPDATE jobs SET state=json_set(state,'$.phase','indexing','$.parse_summary',json(?)),updated_at=? WHERE id=?",params![summary.to_string(),now_rfc3339(),version])?;
    ctx.lock()?.db().execute("UPDATE document_versions SET status='indexing' WHERE id=?", [version])?;
    let (_, index) = child(ctx, invocation, "knowledge-index", json!({"document_versions": [version], "operation": "build"}))?;
    // A stop during the last child must not be committed as a partial import.
    ctx.check()?;
    let indexed = status_of(&index) == "completed";
    if !indexed {
        parts.warnings.extend(child_warnings(&index));
    }
    let chunks: i64 = ctx.lock()?.db().query_row("SELECT COUNT(*) FROM chunks WHERE version_id=?", [version], |r| r.get(0))?;
    let total_blocks: i64 = ctx.lock()?.db().query_row("SELECT COUNT(*) FROM blocks WHERE version_id=?", [version], |r| r.get(0))?;
    let status = if ocr_ok && indexed { "ready" } else { "partial" };
    if status == "partial" {
        parts.status = "partial".into();
    }
    if total_blocks == 0 {
        warn(&mut parts, "no_text_extracted", "未提取到文字：可能是空白页、图片过于模糊或格式受限。");
    }
    ctx.lock()?.db().execute("UPDATE document_versions SET status=? WHERE id=?", params![status, version])?;
    let payload = json!({
        "version_id": version, "version_no": version_no, "display_name": display, "media_kind": kind, "format": format,
        "bytes": bytes, "content_sha256": digest, "status": status, "pages": page_count, "ocr_pages": ocr_pages.len(),
        "native_blocks": written, "blocks": total_blocks, "chunks": chunks, "candidate_facts": candidates,
    });
    Ok((payload, parts))
}

/// DOCX table rows. When the first row is a text header, data rows carry their
/// column names ("金额（元） 5000") so each row stays meaningful as evidence.
fn flush_tables(tables: &mut BTreeMap<(i64, i64), Vec<(i64, String)>>, rows: &mut Vec<BlockRow>) {
    let taken = std::mem::take(tables);
    let headers: BTreeMap<i64, BTreeMap<i64, String>> = taken
        .iter()
        .filter(|((_, row), cells)| *row == 1 && cells.len() >= 2 && cells.iter().filter(|(_, v)| v.parse::<f64>().is_err()).count() * 2 >= cells.len())
        .map(|((table, _), cells)| (*table, cells.iter().cloned().collect()))
        .collect();
    for ((table, row), cells) in taken {
        let names = headers.get(&table).filter(|_| row > 1);
        let text = cells
            .iter()
            .map(|(col, value)| match names.and_then(|n| n.get(col)).filter(|h| *h != value) {
                Some(header) => format!("{header} {value}"),
                None => value.clone(),
            })
            .collect::<Vec<_>>()
            .join(" | ");
        let values: Vec<&String> = cells.iter().map(|(_, v)| v).collect();
        let columns: Vec<i64> = cells.iter().map(|(c, _)| *c).collect();
        rows.push(BlockRow {
            page_id: None,
            kind: "table_row",
            text,
            locator: json!({"type": "docx_table", "table": table, "row": row, "cells": values, "columns": columns, "header_row": row == 1 && headers.contains_key(&table)}),
            confidence: None,
            source: "native",
        });
    }
}

/// Spreadsheet/CSV rows: detect the header row, keep preamble lines as text,
/// and store each data row with its original cells for later column mapping.
fn table_blocks(rows: &mut Vec<BlockRow>, sheet: Option<&str>, collected: Vec<(i64, Vec<Value>)>) {
    // A formula without a cached value is shown as such, never as a number.
    let shown = |c: &Value| -> Option<String> {
        if c["unconfirmed"] == json!(true) {
            return Some("[公式未计算]".into());
        }
        c["value"].as_str().filter(|s| !s.trim().is_empty()).map(str::to_owned)
    };
    let filled = |cells: &Vec<Value>| cells.iter().filter(|c| c["value"].as_str().is_some_and(|s| !s.trim().is_empty())).count();
    let header_index = collected.iter().take(40).position(|(_, cells)| {
        let present: Vec<&str> = cells.iter().filter_map(|c| c["value"].as_str()).filter(|s| !s.trim().is_empty()).collect();
        present.len() >= 3 && present.iter().filter(|s| s.parse::<f64>().is_err()).count() * 2 >= present.len()
    });
    let header: Vec<String> = header_index
        .map(|index| collected[index].1.iter().map(|c| c["value"].as_str().unwrap_or("").to_owned()).collect())
        .unwrap_or_default();
    for (position, (row_no, cells)) in collected.iter().enumerate() {
        if filled(cells) == 0 {
            continue;
        }
        let is_header = Some(position) == header_index;
        let before_header = header_index.is_some_and(|h| position < h);
        let text = if is_header || before_header || header.is_empty() {
            cells.iter().filter_map(shown).collect::<Vec<_>>().join(" | ")
        } else {
            cells.iter().enumerate()
                .filter_map(|(i, c)| shown(c).map(|v| match header.get(i).filter(|h| !h.is_empty()) { Some(h) => format!("{h} {v}"), None => v }))
                .collect::<Vec<_>>()
                .join(" | ")
        };
        let kind = if is_header { "table_header" } else if before_header { "paragraph" } else { "sheet_row" };
        rows.push(BlockRow {
            page_id: None,
            kind,
            text,
            locator: json!({"type": "sheet", "sheet": sheet, "row": row_no, "cells": cells, "header": if kind == "sheet_row" { json!(header) } else { Value::Null }}),
            confidence: None,
            source: "native",
        });
    }
}

/// Rule candidates (amounts/dates) for proofreading; never confirmed facts.
fn extract_candidates(ctx: &Ctx, version: &str, matter: &str) -> Result<usize> {
    let blocks: Vec<(i64, String, String, Option<String>)> = {
        let vault = ctx.lock()?;
        let mut statement = vault.db().prepare("SELECT rowid,text,locator,page_id FROM blocks WHERE version_id=? AND kind NOT IN ('sheet_row','table_header') ORDER BY ordinal")?;
        let rows = statement.query_map([version], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    if blocks.is_empty() {
        return Ok(0);
    }
    let found = local(ctx, json!({"op": "extract_candidates", "blocks": blocks.iter().map(|(_, text, _, _)| json!({"text": text})).collect::<Vec<_>>()}), 120)?;
    let mut vault = ctx.lock()?;
    let candidates = found["candidates"].as_array().cloned().unwrap_or_default();
    let tx = vault.runtime.ledger.db.transaction()?;
    let stale: Vec<String> = {
        let mut statement = tx.prepare("SELECT id FROM facts WHERE source_version=? AND origin='rule_extracted' AND status='candidate'")?;
        let rows = statement.query_map([version], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for id in stale {
        tx.execute("DELETE FROM facts WHERE id=?", [&id])?;
        tx.execute("UPDATE objects SET active=0 WHERE id=?", [&id])?;
    }
    let scope: String = tx.query_row("SELECT scope FROM objects WHERE id=?", [version], |r| r.get(0))?;
    let mut count = 0;
    for candidate in candidates.iter().take(MAX_CANDIDATES) {
        let Some(index) = candidate["block"].as_u64().map(|v| v as usize) else { continue };
        let Some((rowid, _, locator, page)) = blocks.get(index) else { continue };
        let value = match candidate["type"].as_str() {
            Some("amount") => json!({"amount": candidate["value"], "currency": candidate["currency"]}),
            Some("date") => json!({"date": candidate["value"]}),
            _ => continue,
        };
        let source = json!({"block": rowid, "start": candidate["start"], "end": candidate["end"], "page_id": page, "block_locator": serde_json::from_str::<Value>(locator).unwrap_or(Value::Null), "basis": candidate["basis"]});
        let id = crate::store::new_ref();
        let now = now_rfc3339();
        tx.execute("INSERT INTO objects(id,scope,kind,version,active,created_at,parent) VALUES(?,?,'FactRef','0.2.0',1,?,?)", params![id, scope, now, version])?;
        tx.execute(
            "INSERT INTO facts(id,matter_id,fact_type,value,label,source_version,source_locator,status,origin,created_at) VALUES(?,?,?,?,?,?,?,'candidate','rule_extracted',?)",
            params![id, matter, candidate["type"].as_str().unwrap_or(""), value.to_string(), candidate["raw"].as_str().unwrap_or(""), version, source.to_string(), now],
        )?;
        count += 1;
    }
    tx.commit()?;
    Ok(count)
}

// ------------------------------------------------------------------- OCR (5 models)

struct OcrPage {
    id: String,
    page_no: i64,
    blob: String,
    width: f64,
    height: f64,
    turns: u8,
    processed_blob: String,
    grid: Option<Value>,
    polygons: Vec<Value>,
    crops: Vec<(String, Vec<[f64; 2]>)>,
}

fn batches(images: &[String]) -> Vec<std::ops::Range<usize>> {
    let mut ranges = vec![];
    let mut start = 0;
    let mut size = 0;
    for (index, image) in images.iter().enumerate() {
        if index > start && (index - start >= 64 || size + image.len() > IPC_BATCH_CHARS) {
            ranges.push(start..index);
            start = index;
            size = 0;
        }
        size += image.len();
    }
    if start < images.len() {
        ranges.push(start..images.len());
    }
    ranges
}

fn sort_reading_order(items: &mut [(usize, [f64; 4])]) {
    // PaddleX SortQuadBoxes: top-to-bottom, then left-to-right within ~10 px.
    items.sort_by(|a, b| a.1[1].partial_cmp(&b.1[1]).unwrap_or(std::cmp::Ordering::Equal).then(a.1[0].partial_cmp(&b.1[0]).unwrap_or(std::cmp::Ordering::Equal)));
    let length = items.len();
    for i in 0..length.saturating_sub(1) {
        for j in (0..=i).rev() {
            if (items[j + 1].1[1] - items[j].1[1]).abs() < 10.0 && items[j + 1].1[0] < items[j].1[0] {
                items.swap(j, j + 1);
            } else {
                break;
            }
        }
    }
}

/// Retain the exact native spelling when OCR repeats it at the same position.
/// Similar but different text remains available for proofreading.
fn same_native_line(native: &str, native_box: &Value, ocr: &str, bbox: [f64; 4]) -> bool {
    let normalize = |text: &str| text.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    if normalize(native) != normalize(ocr) { return false; }
    let Some(values) = native_box.as_array().filter(|v| v.len() == 4) else { return false };
    let coords: Option<Vec<f64>> = values.iter().map(Value::as_f64).collect();
    let Some(b) = coords else { return false };
    let overlap = (b[2].min(bbox[2]) - b[0].max(bbox[0])).max(0.0) * (b[3].min(bbox[3]) - b[1].max(bbox[1])).max(0.0);
    let smaller = ((b[2] - b[0]) * (b[3] - b[1])).min((bbox[2] - bbox[0]) * (bbox[3] - bbox[1]));
    smaller > 0.0 && overlap / smaller >= 0.5
}

pub fn ocr(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "local.ocr")?;
    ctx.authorize(invocation, "vault.write")?;
    let version = text_of(input, "document_version")?;
    let page_ids = refs_of(input, "page_ids")?;
    let purpose = text_of(input, "purpose")?;
    if page_ids.is_empty() || page_ids.len() > 200 {
        return Err(Error::Contract);
    }
    let unwarp = purpose == "correct";
    let mut pages: Vec<OcrPage> = vec![];
    {
        let vault = ctx.lock()?;
        for page in &page_ids {
            let row: Option<(i64, Option<String>, f64, f64, String)> = vault
                .db()
                .query_row("SELECT page_no,image_blob,width,height,source FROM pages WHERE id=? AND version_id=?", params![page, version], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .optional()?;
            let (page_no, blob, width, height, source) = row.ok_or(Error::Denied)?;
            if !matches!(source.as_str(), "scan" | "image" | "frame") {
                return Err(Error::Contract);
            }
            let blob = blob.ok_or(Error::Integrity)?;
            pages.push(OcrPage { id: page.clone(), page_no, processed_blob: blob.clone(), blob, width, height, turns: 0, grid: None, polygons: vec![], crops: vec![] });
        }
    }
    let total = pages.len() as u64;
    let image_b64 = |ctx: &Ctx, blob: &str| -> Result<String> { Ok(STANDARD.encode(&*ctx.lock()?.get_blob(blob)?)) };
    // 1. Whole-page orientation (PP-LCNet doc_ori), then exact quarter turns.
    for (done, page) in pages.iter_mut().enumerate() {
        ctx.report(invocation, "orientation", done as u64, total);
        let encoded = image_b64(ctx, &page.blob)?;
        let result = ctx.services.predict("doc-ori", &json!({"kind": "doc-ori", "png_base64": encoded}), ctx.cancel)?;
        let label = result["labels"].get(0).and_then(Value::as_str).unwrap_or("0").to_owned();
        let turns = local(ctx, json!({"op": "doc_ori_turns", "label": label}), 30)?["clockwise_turns"].as_u64().unwrap_or(0) as u8;
        page.turns = turns;
        if turns != 0 {
            let rotated = local(ctx, json!({"op": "rotate_quarter", "png_base64": encoded, "clockwise_turns": turns}), 120)?;
            let bytes = png_bytes(&rotated)?;
            let vault = ctx.lock()?;
            page.processed_blob = vault.put_blob(&page.id, "page_rotated", &bytes)?;
        }
    }
    // 2. Optional UVDoc unwarping, only when the user asked for correction.
    if unwarp {
        for (done, page) in pages.iter_mut().enumerate() {
            ctx.report(invocation, "unwarp", done as u64, total);
            let encoded = image_b64(ctx, &page.processed_blob)?;
            let result = ctx.services.predict("uvdoc", &json!({"kind": "uvdoc", "png_base64": encoded}), ctx.cancel)?;
            if result["geometry_mapping_verified"] != json!(true) {
                return Err(Error::Integrity);
            }
            BackwardGrid::from_worker(&result["geometry"])?;
            let bytes = png_bytes(&result)?;
            let vault = ctx.lock()?;
            page.processed_blob = vault.put_blob(&page.id, "page_unwarped", &bytes)?;
            page.grid = Some(result["geometry"].clone());
        }
    }
    // 3. Text detection per page.
    for (done, page) in pages.iter_mut().enumerate() {
        ctx.report(invocation, "detection", done as u64, total);
        let encoded = image_b64(ctx, &page.processed_blob)?;
        let result = ctx.services.predict("ocr-det", &json!({"kind": "ocr-det", "png_base64": encoded}), ctx.cancel)?;
        page.polygons = result["polygons"].as_array().cloned().unwrap_or_default();
        if !page.polygons.is_empty() {
            let crops = local(ctx, json!({"op": "crop_polygons", "png_base64": encoded, "polygons": page.polygons}), 300)?;
            for crop in crops["crops"].as_array().cloned().unwrap_or_default() {
                if crop.is_null() {
                    continue;
                }
                let quad: Vec<[f64; 2]> = crop["quad"].as_array().cloned().unwrap_or_default().iter()
                    .filter_map(|p| Some([p.get(0)?.as_f64()?, p.get(1)?.as_f64()?]))
                    .collect();
                if quad.len() == 4 {
                    page.crops.push((crop["png_base64"].as_str().unwrap_or("").to_owned(), quad));
                }
            }
        }
    }
    // 4. Text-line orientation (0/180) for all crops, batched.
    let all: Vec<(usize, usize)> = pages.iter().enumerate().flat_map(|(p, page)| (0..page.crops.len()).map(move |c| (p, c))).collect();
    let images: Vec<String> = all.iter().map(|(p, c)| pages[*p].crops[*c].0.clone()).collect();
    let mut flips = vec![false; images.len()];
    for range in batches(&images) {
        ctx.check()?;
        ctx.report(invocation, "line_orientation", range.start as u64, images.len() as u64);
        let result = ctx.services.predict("textline-ori", &json!({"kind": "textline-ori", "images": images[range.clone()].to_vec()}), ctx.cancel)?;
        for (offset, item) in result["results"].as_array().ok_or(Error::Contract)?.iter().enumerate() {
            flips[range.start + offset] = item["class_ids"].get(0).and_then(Value::as_i64) == Some(1);
        }
    }
    // 5. Recognition (PP-OCRv6 medium rec), batched with 180-degree flags.
    let mut texts: Vec<(String, f64)> = vec![(String::new(), 0.0); images.len()];
    for range in batches(&images) {
        ctx.check()?;
        ctx.report(invocation, "recognition", range.start as u64, images.len() as u64);
        let result = ctx.services.predict("ocr-rec", &json!({"kind": "ocr-rec", "images": images[range.clone()].to_vec(), "rotate180": flips[range.clone()].to_vec()}), ctx.cancel)?;
        for (offset, item) in result["results"].as_array().ok_or(Error::Contract)?.iter().enumerate() {
            texts[range.start + offset] = (item["text"].as_str().unwrap_or("").to_owned(), item["score"].as_f64().unwrap_or(0.0));
        }
    }
    // 6. Map every quadrilateral back to the original page and store lines.
    let mut rows = vec![];
    let mut page_reports = vec![];
    let mut cursor = 0;
    for page in &pages {
        let grid = page.grid.as_ref().map(BackwardGrid::from_worker).transpose()?;
        let (w, h) = (page.width.max(1.0), page.height.max(1.0));
        let processed_size = if page.turns % 2 == 1 { (h, w) } else { (w, h) };
        let mut items: Vec<(usize, [f64; 4])> = vec![];
        let mut mapped: Vec<(Vec<[f64; 2]>, &'static str)> = vec![];
        for (index, (_, quad)) in page.crops.iter().enumerate() {
            let mut frame = "original";
            let mut points = vec![];
            for corner in quad {
                let clamped = Point { x: corner[0].clamp(0.0, processed_size.0 - 1.0), y: corner[1].clamp(0.0, processed_size.1 - 1.0) };
                let source = match &grid {
                    Some(grid) => grid.to_source(clamped)?,
                    None => Some(clamped),
                };
                let original = match source {
                    Some(point) => unrotate(point, [w as u32, h as u32], page.turns)?,
                    None => {
                        frame = "rectified";
                        clamped
                    }
                };
                points.push([original.x, original.y]);
            }
            let (fw, fh) = if frame == "original" { (w, h) } else { processed_size };
            let normalized: Vec<[f64; 2]> = points.iter().map(|p| [(p[0] / fw).clamp(0.0, 1.0), (p[1] / fh).clamp(0.0, 1.0)]).collect();
            let xs = normalized.iter().map(|p| p[0]);
            let ys = normalized.iter().map(|p| p[1]);
            let bbox = [xs.clone().fold(1.0, f64::min), ys.clone().fold(1.0, f64::min), xs.fold(0.0, f64::max), ys.fold(0.0, f64::max)];
            let top = quad.iter().map(|p| p[1]).fold(f64::MAX, f64::min);
            let left = quad.iter().map(|p| p[0]).fold(f64::MAX, f64::min);
            items.push((index, [left, top, 0.0, 0.0]));
            mapped.push((vec![[bbox[0], bbox[1]], [bbox[2], bbox[3]]].into_iter().chain(normalized).collect(), frame));
        }
        sort_reading_order(&mut items);
        let native: Vec<(String, Value)> = {
            let vault = ctx.lock()?;
            let mut statement = vault.db().prepare("SELECT text,locator FROM blocks WHERE page_id=? AND source='native'")?;
            let values = statement.query_map([&page.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            values.into_iter().map(|(text, locator)| Ok((text, serde_json::from_str(&locator)?))).collect::<Result<_>>()?
        };
        let mut low = 0;
        let mut kept = 0;
        let mut recognized = 0;
        for (index, _) in &items {
            let (text, score) = &texts[cursor + index];
            if text.trim().is_empty() {
                continue;
            }
            let (geometry, frame) = &mapped[*index];
            recognized += 1;
            let bbox = [geometry[0][0], geometry[0][1], geometry[1][0], geometry[1][1]];
            if frame == &"original" && native.iter().any(|(original, locator)| same_native_line(original, &locator["bbox"], text, bbox)) {
                continue;
            }
            if *score < 0.8 {
                low += 1;
            }
            kept += 1;
            rows.push(BlockRow {
                page_id: Some(page.id.clone()),
                kind: "ocr_line",
                text: text.clone(),
                locator: json!({"type": "page", "page_no": page.page_no, "page_id": page.id, "bbox": [geometry[0][0], geometry[0][1], geometry[1][0], geometry[1][1]],
                                "polygon": geometry[2..].to_vec(), "frame": frame, "rotation_turns": page.turns, "unwarped": page.grid.is_some(), "low_confidence": *score < 0.8}),
                confidence: Some(*score),
                source: "ocr",
            });
        }
        cursor += page.crops.len();
        page_reports.push(json!({"page_id": page.id, "page_no": page.page_no, "lines": kept, "recognized_lines": recognized, "native_lines": native.len(), "low_confidence_lines": low, "clockwise_turns": page.turns, "unwarped": page.grid.is_some()}));
    }
    insert_blocks(ctx, &version, &rows, Some(&page_ids))?;
    {
        let vault = ctx.lock()?;
        for page in &pages {
            vault.db().execute(
                "UPDATE pages SET ocr_status='done',rotation_turns=?,uvdoc=?,derived_blob=?,geometry=? WHERE id=?",
                params![i64::from(page.turns), i64::from(page.grid.is_some()), if page.processed_blob != page.blob { Some(page.processed_blob.clone()) } else { None }, page.grid.as_ref().map(Value::to_string), page.id],
            )?;
        }
    }
    let unreadable: Vec<&Value> = page_reports.iter().filter(|p| p["recognized_lines"] == json!(0)).collect();
    let models: Vec<&str> = ["doc-ori", if unwarp { "uvdoc" } else { "" }, "ocr-det", "textline-ori", "ocr-rec"].into_iter().filter(|m| !m.is_empty()).collect();
    let payload = json!({"version_id": version, "pages": page_reports, "lines": rows.len(), "models": models});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "OcrResultRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.processed = page_ids.clone();
    if !unreadable.is_empty() {
        warn(&mut parts, "page_without_text", "部分页面未识别到文字，请对照原页确认是否为空白或图像过于模糊。");
    }
    if rows.iter().any(|r| r.locator["low_confidence"] == json!(true)) {
        warn(&mut parts, "low_confidence_lines", "部分文字识别置信度较低；金额、日期和姓名请回到原图核对。");
    }
    Ok(parts)
}

// ------------------------------------------------------------------ evidence-read

/// Ensure a displayable page image exists (renders native PDF pages lazily).
pub fn ensure_display_image(ctx: &Ctx, page: &str) -> Result<String> {
    let (image, version, page_no, source): (Option<String>, String, i64, String) =
        ctx.lock()?.db().query_row("SELECT image_blob,version_id,page_no,source FROM pages WHERE id=?", [page], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    if let Some(blob) = image {
        return Ok(blob);
    }
    if source != "native" {
        return Err(Error::Integrity);
    }
    let original: String = ctx.lock()?.db().query_row("SELECT original_blob FROM document_versions WHERE id=?", [&version], |r| r.get(0))?;
    let data = ctx.lock()?.get_blob(&original)?;
    let upload = ctx.services.local.upload(&data, ctx.cancel)?;
    let rendered = local(ctx, json!({"op": "pdf_render", "upload_id": upload.id, "first": page_no, "purpose": "display"}), 300)?;
    let bytes = png_bytes(&rendered)?;
    let vault = ctx.lock()?;
    let blob = vault.put_blob(page, "page_display", &bytes)?;
    vault.db().execute("UPDATE pages SET image_blob=? WHERE id=?", params![blob, page])?;
    Ok(blob)
}

/// Preserve the source of every quoted span, including chunks made before
/// OCR and ASR were split into separate index groups. Never infer provenance
/// from the first page of a mixed chunk.
pub(crate) fn source_parts(vault: &crate::store::Vault, version: &str, spans: &[Value]) -> Result<Vec<Value>> {
    let mut parts: Vec<Value> = vec![];
    for span in spans {
        let block = span["block"].as_i64().ok_or(Error::Integrity)?;
        let (text, source, locator, page, page_source): (String, String, String, Option<String>, Option<String>) = vault.db().query_row(
            "SELECT b.text,b.source,b.locator,b.page_id,p.source FROM blocks b LEFT JOIN pages p ON p.id=b.page_id WHERE b.rowid=? AND b.version_id=?",
            params![block, version], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?.ok_or_else(|| Error::Worker("source_changed".into()))?;
        let start = span["start"].as_u64().unwrap_or(0) as usize;
        let end = span["end"].as_u64().unwrap_or(text.chars().count() as u64) as usize;
        let text: String = text.chars().skip(start).take(end.saturating_sub(start)).collect();
        let mut locator: Value = serde_json::from_str(&locator)?;
        let (kind, label) = if source == "asr" {
            ("audio_asr", format!("音轨转写（本机 ASR，{:.1}–{:.1} 秒）", locator["start_s"].as_f64().unwrap_or(0.0), locator["end_s"].as_f64().unwrap_or(0.0)))
        } else if page_source.as_deref() == Some("frame") {
            let time: Option<f64> = vault.db().query_row("SELECT time_s FROM frames WHERE page_id=? ORDER BY seq LIMIT 1", [&page], |r| r.get(0)).optional()?;
            if let Some(time) = time { locator["time_s"] = json!(time); }
            ("frame_ocr", match time { Some(t) => format!("画面文字（本机 OCR，{t:.1} 秒）"), None => "画面文字（本机 OCR）".into() })
        } else if source == "ocr" { ("image_ocr", "图片文字（本机 OCR）".into()) }
        else { ("native", "文档正文".into()) };
        if matches!(kind, "frame_ocr" | "image_ocr") {
            if let Some(previous) = parts.last_mut().filter(|p| p["kind"] == kind && p["page_id"] == json!(page)) {
                previous["text"] = json!(format!("{}\n{text}", previous["text"].as_str().unwrap_or("")));
                previous["locators"].as_array_mut().ok_or(Error::Integrity)?.push(locator);
                continue;
            }
        }
        parts.push(json!({"kind": kind, "label": label, "text": text, "locators": [locator.clone()], "locator": locator, "page_id": page}));
    }
    Ok(parts)
}

pub fn evidence_read(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let ids = refs_of(input, "evidence_ids")?;
    let mode = text_of(input, "context_mode")?;
    if ids.is_empty() || ids.len() > 20 {
        return Err(Error::Contract);
    }
    let mut items = vec![];
    let mut page_set: Vec<String> = vec![];
    for id in &ids {
        let (version, matter, unit, unit_title, body, spans): (String, String, i64, String, String, String) = ctx.lock()?.db().query_row(
            "SELECT c.version_id,c.matter_id,c.unit,c.unit_title,c.body,c.spans FROM chunks c JOIN objects o ON o.id=c.id WHERE c.id=? AND o.active=1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        ).optional()?.ok_or(Error::Denied)?;
        let (display, version_no, media_kind): (String, i64, String) = ctx.lock()?.db().query_row(
            "SELECT d.display_name,v.version_no,v.media_kind FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?",
            [&version],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let span_rows: Vec<Value> = serde_json::from_str(&spans)?;
        let source_parts = source_parts(&*ctx.lock()?, &version, &span_rows)?;
        let block_ids: Vec<i64> = match mode.as_str() {
            "parent_clause" => {
                let vault = ctx.lock()?;
                let mut statement = vault.db().prepare("SELECT spans FROM chunks WHERE version_id=? AND unit=? ORDER BY ordinal")?;
                let rows = statement.query_map(params![version, unit], |r| r.get::<_, String>(0))?;
                let mut ids = vec![];
                for row in rows {
                    for span in serde_json::from_str::<Vec<Value>>(&row?)? {
                        if let Some(block) = span["block"].as_i64() {
                            if !ids.contains(&block) {
                                ids.push(block);
                            }
                        }
                    }
                }
                ids
            }
            _ => span_rows.iter().filter_map(|s| s["block"].as_i64()).collect(),
        };
        let mut locators = vec![];
        let mut texts = vec![];
        let mut pages = vec![];
        {
            let vault = ctx.lock()?;
            for block in &block_ids {
                let (text, locator, page): (String, String, Option<String>) =
                    vault.db().query_row("SELECT text,locator,page_id FROM blocks WHERE rowid=? AND version_id=?", params![block, version], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                texts.push(text);
                locators.push(serde_json::from_str::<Value>(&locator)?);
                if let Some(page) = page {
                    if !pages.contains(&page) {
                        pages.push(page);
                    }
                }
            }
        }
        if mode == "page" {
            let vault = ctx.lock()?;
            texts.clear();
            locators.clear();
            for page in &pages {
                let mut statement = vault.db().prepare("SELECT text,locator FROM blocks WHERE page_id=? ORDER BY ordinal")?;
                let rows = statement.query_map([page], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                for row in rows {
                    let (text, locator) = row?;
                    texts.push(text);
                    locators.push(serde_json::from_str::<Value>(&locator)?);
                }
            }
        }
        for page in &pages {
            ensure_display_image(ctx, page)?;
            if !page_set.contains(page) {
                page_set.push(page.clone());
            }
        }
        let highlight: Vec<Value> = span_rows.iter().filter_map(|s| s["block"].as_i64()).filter_map(|b| block_ids.iter().position(|x| *x == b)).map(|p| json!(p)).collect();
        let legal:Option<String>=ctx.lock()?.db().query_row("SELECT metadata FROM legal_sources WHERE version_id=?",[&version],|r|r.get(0)).optional()?;
        items.push(json!({"evidence_id": id, "version_id": version, "matter_id": matter, "document": {"display_name": display, "version_no": version_no, "media_kind": media_kind},"legal_source":legal.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,
            "unit_title": unit_title, "exact_text": body, "text": texts.join("\n"), "source_parts": source_parts, "locators": locators, "highlight_indexes": highlight, "pages": pages}));
    }
    let payload = json!({"mode": mode, "items": items, "pages": page_set, "content_hash": sha256_hex(serde_json::to_string(&items)?.as_bytes())});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "EvidenceReadRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.evidence_refs = ids.clone();
    parts.processed = ids;
    Ok(parts)
}
