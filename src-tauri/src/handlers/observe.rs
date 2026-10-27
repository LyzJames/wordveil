//! media-observe / direct_media (multimodal design 2, 4; contract 3): the
//! model reads the approved native media itself — images when the connection
//! passed the vision self-test, audio when it passed the audio self-test. The
//! first read never sees the local OCR/ASR text; the core then compares the
//! key fields the model reports with the local extraction of the same source.
//! Native video and joint media inputs are not sent by this version.
use super::*;
use super::answers::{Call, date_supported, generate_json, generation_available, grant_for_root, key_dates, key_numbers, system_prompt};
use crate::{generation::{Part, connections}, vault::now_rfc3339};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const MAX_IMAGES_PER_REQUEST: usize = 12;
const VISUAL_SAMPLES: f64 = 24.0;

struct Media {
    id: String,
    kind: String,
    mime: String,
    blob: String,
    source: String,
    detail: Value,
}

fn describe(media: &Media, label: &str) -> String {
    match media.kind.as_str() {
        "audio" => format!("{label}：音频《{}》{:.0}–{:.0} 秒", media.detail["display"].as_str().unwrap_or(""), media.detail["start_s"].as_f64().unwrap_or(0.0), media.detail["end_s"].as_f64().unwrap_or(0.0)),
        _ => format!("{label}：图片《{}》{}", media.detail["display"].as_str().unwrap_or(""), media.detail["location"].as_str().unwrap_or("")),
    }
}

/// Local extraction of the same source: OCR lines of a page, or the merged
/// transcript of the audio range.
fn extracted_text(ctx: &Ctx, media: &Media) -> Result<String> {
    let vault = ctx.lock()?;
    Ok(match media.kind.as_str() {
        "audio" => {
            let mut statement = vault.db().prepare("SELECT text FROM blocks WHERE version_id=? AND source='asr' ORDER BY ordinal")?;
            statement.query_map([media.detail["version_id"].as_str().unwrap_or("")], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?.join("\n")
        }
        _ => {
            let mut statement = vault.db().prepare("SELECT text FROM blocks WHERE page_id=? AND source='ocr' ORDER BY ordinal")?;
            statement.query_map([&media.source], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?.join("\n")
        }
    })
}

fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace() && !matches!(c, ',' | '，' | '、')).collect()
}

/// Field-by-field comparison (design 4.1 step 3): agreement is only a check
/// signal, never proof; a value the local extraction lacks is a disagreement.
fn compare(ctx: &Ctx, media: &[Media], fields: &[Value]) -> Result<Value> {
    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    for item in media {
        texts.insert(item.id.clone(), extracted_text(ctx, item)?);
    }
    let (mut agreements, mut disagreements) = (vec![], vec![]);
    for field in fields {
        let media_ref = field["media_ref"].as_str().unwrap_or("");
        let name = field["name"].as_str().unwrap_or("");
        let value = field["value"].as_str().unwrap_or("");
        let source = texts.get(media_ref).cloned().unwrap_or_default();
        // Amounts and dates are compared as values: Arabic digits and Chinese
        // numerals both normalise ("两万七" = 27000), and a date without a year
        // ("5月6号") matches the same month and day. Anything the normaliser
        // does not read is compared as text, so a spelling is not a conflict.
        let as_text = || !compact(value).is_empty() && compact(&source).contains(&compact(value));
        let found = match name {
            "amount" => {
                let observed = key_numbers(value);
                if observed.is_empty() { as_text() } else { observed.iter().all(|n| key_numbers(&source).contains(n)) }
            }
            "date" => {
                let observed = key_dates(value);
                if observed.is_empty() { as_text() } else { observed.iter().all(|d| date_supported(d, &key_dates(&source))) }
            }
            _ => as_text(),
        };
        let row = json!({"media_ref": media_ref, "name": name, "observed": value, "extracted": if found { json!(value) } else { Value::Null }});
        if found { agreements.push(row) } else { disagreements.push(row) }
    }
    Ok(json!({"agreements": agreements, "disagreements": disagreements, "method": "确定性字段比对：金额按数值比较（阿拉伯数字与中文数字、口语省略写法如“两万七”统一换算）；日期归一化后比较，不带年份的日期按月日比较；其余字段按去空白后的原文包含关系"}))
}

pub fn media_observe(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    if !generation_available(ctx, invocation)? {
        let mut parts = EnvelopeParts { status: "requires_generation".into(), ..Default::default() };
        warn(&mut parts, "requires_generation", "直接读取媒体需要已授权、并通过对应能力自检的生成接口；已按本机识别文字处理。");
        return Ok(parts);
    }
    let ids = refs_of(input, "media_refs")?;
    let question = text_of(input, "question_id")?;
    let mode = text_of(input, "observation_mode")?;
    if ids.is_empty() || ids.len() > MAX_IMAGES_PER_REQUEST {
        return Err(Error::Contract);
    }
    let grant = grant_for_root(ctx, invocation.root_id())?.ok_or(Error::Denied)?;
    let media: Vec<Media> = {
        let vault = ctx.lock()?;
        let mut out = vec![];
        for id in &ids {
            if !grant.media.contains(id) {
                return Err(Error::Denied);
            }
            let (kind, mime, blob, source, detail): (String, String, String, String, String) = vault.db()
                .query_row("SELECT kind,mime,blob,source,detail FROM native_media WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .optional()?.ok_or(Error::Denied)?;
            out.push(Media { id: id.clone(), kind, mime, blob, source, detail: serde_json::from_str(&detail)? });
        }
        out
    };
    // Only modalities the connection really passed in its self-test are sent.
    {
        let vault = ctx.lock()?;
        let connection = connections::load(&vault, &grant.connection_id)?;
        for kind in media.iter().map(|m| m.kind.as_str()).collect::<std::collections::BTreeSet<_>>() {
            let capability = if kind == "audio" { "audio_input" } else { "vision" };
            if connections::capability(&vault, &connection, capability)? != "supported" {
                drop(vault);
                let mut parts = EnvelopeParts { status: "requires_generation".into(), ..Default::default() };
                warn(&mut parts, "native_media_unsupported", "该生成接口没有通过对应的原生媒体能力自检；未上传媒体，按本机识别文字处理。");
                return Ok(parts);
            }
        }
    }
    let images = media.iter().filter(|m| m.kind == "image").count();
    if images > 0 {
        super::mediaflow::charge(ctx, invocation.root_id(), "native_visual_samples", images as f64, VISUAL_SAMPLES)?;
    }
    let labels: Vec<(String, &Media)> = media.iter().enumerate().map(|(i, m)| (format!("M{}", i + 1), m)).collect();
    let question_text: String = ctx.lock()?.db().query_row("SELECT text FROM questions WHERE id=?", [&question], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
    let listing: String = labels.iter().map(|(label, m)| format!("{}\n", describe(m, label))).collect();
    let (system, prompt, prompt_sha, comparison) = if mode == "targeted_compare" {
        // A second, targeted look at fields the comparison of this root's
        // first reading found inconsistent, for the media that had them.
        let reference = text_of(input, "comparison_ref")?;
        let (reference, previous) = super::flows::owned_result(ctx, &json!(reference), "MediaObservationRef", invocation.root_id())?.ok_or(Error::Denied)?;
        let disagreeing: Vec<&str> = previous["comparison"]["disagreements"].as_array().map(|rows| rows.iter().filter_map(|r| r["media_ref"].as_str()).collect()).unwrap_or_default();
        if previous["mode"] != "first_read" || ids.iter().any(|id| !disagreeing.contains(&id.as_str())) {
            return Err(Error::Denied);
        }
        let (system, prompt, sha) = system_prompt(ctx, &["direct_media.compare"])?;
        (system, prompt, sha, Some((reference, previous)))
    } else {
        let (system, prompt, sha) = system_prompt(ctx, &["direct_media.first_read"])?;
        (system, prompt, sha, None)
    };
    let mut user = format!("原问题：{question_text}\n\n授权读取的媒体：\n{listing}");
    if let Some((_, previous)) = &comparison {
        user.push_str("\n需要重新核看的字段：\n");
        for row in previous["comparison"]["disagreements"].as_array().cloned().unwrap_or_default() {
            let Some(label) = labels.iter().find(|(_, m)| row["media_ref"] == m.id.as_str()).map(|(l, _)| l.clone()) else { continue };
            user.push_str(&format!("- {label} {}：你上次读到“{}”，本机识别文字中没有找到这个值\n", row["name"].as_str().unwrap_or(""), row["observed"].as_str().unwrap_or("")));
        }
    }
    user.push_str("\n返回 JSON：{\"observations\": [{\"media\": \"0.2.0\", \"text\": \"…\"}], \"fields\": [{\"media\": \"0.2.0\", \"name\": \"amount|date|name|account|id_number\", \"value\": \"原样照录\"}], \"limitations\": [\"…\"]}");
    let mut parts_out = vec![];
    for (_, item) in &labels {
        let bytes = ctx.lock()?.get_blob(&item.blob)?;
        let encoded = STANDARD.encode(&*bytes);
        parts_out.push(match item.kind.as_str() {
            "audio" => Part::AudioWav(encoded),
            _ => Part::ImagePng(format!("data:{};base64,{encoded}", item.mime)),
        });
    }
    let call = Call { calculations: vec![], purpose: "direct_media", prompt, prompt_sha, question: question.clone(), evidence: vec![], media: ids.clone() };
    let (wire, request_id, response) = generate_json(ctx, invocation, &call, &system, &user, parts_out, 1500)?;
    let by_label: BTreeMap<String, String> = labels.iter().map(|(l, m)| (l.clone(), m.id.clone())).collect();
    let resolve = |label: &Value| by_label.get(&label.as_str().unwrap_or("").trim().to_ascii_uppercase()).cloned();
    let observations: Vec<Value> = wire["observations"].as_array().cloned().unwrap_or_default().iter().filter_map(|row| {
        let media = resolve(&row["media"])?;
        Some(json!({"media_ref": media, "text": row["text"]}))
    }).collect();
    let fields: Vec<Value> = wire["fields"].as_array().cloned().unwrap_or_default().iter().filter_map(|row| {
        let media = resolve(&row["media"])?;
        Some(json!({"media_ref": media, "name": row["name"], "value": row["value"]}))
    }).collect();
    let texts: Vec<Value> = observations.iter().map(|o| {
        let label = labels.iter().find(|(_, m)| o["media_ref"] == m.id.as_str()).map(|(l, _)| l.clone()).unwrap_or_default();
        json!(format!("{label}：{}", o["text"].as_str().unwrap_or("")))
    }).collect();
    let observation = json!({"media_refs": ids, "observations": texts, "limitations": wire["limitations"]});
    ctx.lock()?.runtime.validate_generation_output(invocation, &observation)?;
    let supersedes = comparison.as_ref().map(|(reference, _)| reference.clone());
    let comparison = compare(ctx, &media, &fields)?;
    let payload = json!({"observation": observation, "observed": observations, "fields": fields, "comparison": comparison, "mode": mode, "supersedes": supersedes,
        "media": labels.iter().map(|(l, m)| json!({"label": l, "media_ref": m.id, "kind": m.kind, "detail": m.detail})).collect::<Vec<_>>(),
        "directly_observed": true, "request_id": request_id, "prompt": format!("{}@{}", call.prompt, call.prompt_sha),
        "usage": {"input_tokens": response.usage.input_tokens, "output_tokens": response.usage.output_tokens}, "created_at": now_rfc3339()});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "MediaObservationRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.processed = ids;
    Ok(parts)
}

/// Core evidence registration for media (contract 3): the indexed chunks of
/// the given pages (or whole versions) as an EvidenceBundleRef of `invocation`.
pub(crate) fn media_bundle(ctx: &Ctx, invocation: &Invocation, versions: &[String], pages: &[String]) -> Result<Option<String>> {
    let vault = ctx.lock()?;
    let mut items = vec![];
    for version in versions {
        let mut statement = vault.db().prepare("SELECT c.id,c.matter_id,c.spans FROM chunks c JOIN objects o ON o.id=c.id WHERE c.version_id=? AND o.active=1 ORDER BY c.ordinal")?;
        let rows = statement.query_map([version], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        for row in rows {
            let (id, matter, spans) = row?;
            if !pages.is_empty() {
                let blocks: Vec<i64> = serde_json::from_str::<Vec<Value>>(&spans)?.iter().filter_map(|s| s["block"].as_i64()).collect();
                let mut on_page = false;
                for block in blocks {
                    let page: Option<String> = vault.db().query_row("SELECT page_id FROM blocks WHERE rowid=?", [block], |r| r.get(0)).optional()?.flatten();
                    on_page |= page.is_some_and(|p| pages.contains(&p));
                }
                if !on_page {
                    continue;
                }
            }
            items.push(json!({"evidence_id": id, "version_id": version, "matter_id": matter, "source": "media_projection"}));
        }
    }
    if items.is_empty() {
        return Ok(None);
    }
    let take = items.len().min(10);
    let payload = json!({"question_ref": null, "candidate_set_id": null, "model": "core media projection (no ranking)", "pairs": 0, "initial_take": take, "items": items});
    Ok(Some(vault.put_result(invocation.skill(), invocation.id(), "EvidenceBundleRef", &payload)?))
}

/// Delete the model copies (JPEG/WAV) prepared for one grant: rows, blobs and
/// object records. They exist only to be sent in that one task.
pub(crate) fn release_native(vault: &crate::store::Vault, grant: &str) -> Result<usize> {
    let rows: Vec<(String, String)> = {
        let mut statement = vault.db().prepare("SELECT id,blob FROM native_media WHERE grant_id=?")?;
        statement.query_map([grant], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (id, blob) in &rows {
        vault.delete_blob(blob)?;
        vault.db().execute("DELETE FROM native_media WHERE id=?", [id])?;
        vault.db().execute("UPDATE objects SET active=0 WHERE id=?", [id])?;
    }
    Ok(rows.len())
}

/// A task of `root` ended for good (not interrupted): its copies go.
pub(crate) fn release_root_native(vault: &crate::store::Vault, root: &str) -> Result<usize> {
    let resumable: bool = vault.db().query_row("SELECT EXISTS(SELECT 1 FROM flow_runs WHERE root=? AND status IN ('interrupted','running','resuming'))", [root], |r| r.get(0))?;
    if resumable {
        return Ok(0);
    }
    let grants: Vec<String> = {
        let mut statement = vault.db().prepare("SELECT id FROM egress_grants WHERE root=?")?;
        statement.query_map([root], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let mut released = 0;
    for grant in grants {
        released += release_native(vault, &grant)?;
    }
    Ok(released)
}

/// After unlocking: previews left unapproved by the last session can no
/// longer be confirmed, and copies of tasks that cannot be continued are
/// removed (an interrupted run keeps them until it is continued or discarded).
pub(crate) fn sweep_native(vault: &crate::store::Vault) -> Result<usize> {
    vault.db().execute("UPDATE egress_grants SET revoked_at=? WHERE approved_at IS NULL AND revoked_at IS NULL", [now_rfc3339()])?;
    let grants: Vec<String> = {
        let mut statement = vault.db().prepare(
            "SELECT DISTINCT n.grant_id FROM native_media n LEFT JOIN egress_grants g ON g.id=n.grant_id
             WHERE g.id IS NULL OR g.revoked_at IS NOT NULL OR g.root IS NULL
                OR NOT EXISTS (SELECT 1 FROM flow_runs f WHERE f.root=g.root AND f.status IN ('interrupted','running','resuming'))",
        )?;
        statement.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let mut released = 0;
    for grant in grants {
        released += release_native(vault, &grant)?;
    }
    Ok(released)
}

/// Remember the approved native media of a grant (prepared at preview).
pub(crate) fn register_native(vault: &crate::store::Vault, grant: &str, source: &str, kind: &str, mime: &str, bytes: &[u8], detail: &Value) -> Result<String> {
    let id = vault.register("NativeMediaRef", "0.3.0", Some(source))?;
    let blob = vault.put_blob(&id, "native_media", bytes)?;
    vault.db().execute(
        "INSERT INTO native_media(id,grant_id,source,kind,mime,bytes,sha256,blob,detail,created_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
        params![id, grant, source, kind, mime, bytes.len() as i64, crate::store::sha256_hex(bytes), blob, detail.to_string(), now_rfc3339()],
    )?;
    Ok(id)
}

/// Prepare the media a grant's policy approved for this run (audio/video
/// review): evenly spread frames up to the policy count, and the range's audio
/// up to its seconds limit. Idempotent: a continued run reuses what exists.
pub(crate) fn prepare_run_media(ctx: &Ctx, grant: &super::answers::Grant, frames: &[String], audio: Option<&str>) -> Result<()> {
    let existing: i64 = ctx.lock()?.db().query_row("SELECT COUNT(*) FROM native_media WHERE grant_id=?", [&grant.id], |r| r.get(0))?;
    if existing > 0 || grant.policy.is_null() {
        return Ok(());
    }
    let limit = grant.policy["frames"].as_u64().unwrap_or(0).min(MAX_IMAGES_PER_REQUEST as u64) as usize;
    if limit > 0 && !frames.is_empty() {
        let step = (frames.len() as f64 / limit as f64).max(1.0);
        let picks: Vec<&String> = (0..limit.min(frames.len())).map(|i| &frames[((i as f64) * step) as usize]).collect();
        for page in picks {
            let (blob, display): (String, String) = ctx.lock()?.db().query_row(
                "SELECT p.image_blob,d.display_name FROM pages p JOIN document_versions v ON v.id=p.version_id JOIN documents d ON d.id=v.document_id WHERE p.id=? AND p.image_blob IS NOT NULL",
                [page], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let png = ctx.lock()?.get_blob(&blob)?;
            let encoded = ctx.services.local.call(&json!({"op": "encode_for_model", "png_base64": STANDARD.encode(&*png), "max_side": 2048, "quality": 85}), ctx.cancel, std::time::Duration::from_secs(120))?;
            let jpeg = STANDARD.decode(encoded["jpeg_base64"].as_str().ok_or(Error::Contract)?).map_err(|_| Error::Contract)?;
            let time: Option<f64> = ctx.lock()?.db().query_row("SELECT time_s FROM frames WHERE page_id=?", [page], |r| r.get(0)).optional()?;
            let detail = json!({"display": display, "location": format!("视频画面 {:.1} 秒", time.unwrap_or(0.0)), "width": encoded["width"], "height": encoded["height"]});
            let vault = ctx.lock()?;
            register_native(&vault, &grant.id, page, "image", "image/jpeg", &jpeg, &detail)?;
        }
    }
    let seconds = grant.policy["audio_seconds"].as_f64().unwrap_or(0.0).min(crate::service::AUDIO_MAX_SECONDS);
    if let (Some(media), true) = (audio, seconds > 0.0) {
        let (rows, version, display): (Vec<(f64, f64, String)>, String, String) = {
            let vault = ctx.lock()?;
            let mut statement = vault.db().prepare("SELECT start_s,end_s,pcm_blob FROM audio_segments WHERE media_id=? ORDER BY seq")?;
            let rows = statement.query_map([media], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let (version, display): (String, String) = vault.db().query_row("SELECT v.id,d.display_name FROM media m JOIN document_versions v ON v.id=m.version_id JOIN documents d ON d.id=v.document_id WHERE m.id=?", [media], |r| Ok((r.get(0)?, r.get(1)?)))?;
            (rows, version, display)
        };
        // Windows overlap by one second; keep each sample once, up to the limit.
        let mut samples: Vec<i16> = vec![];
        let mut covered = f64::MIN;
        let (mut first, mut last) = (None, 0.0);
        for (start, end, blob) in rows {
            let pcm = ctx.lock()?.get_blob(&blob)?;
            let skip = if covered > start { ((covered - start) * 16_000.0) as usize } else { 0 };
            for chunk in pcm.chunks_exact(4).skip(skip) {
                if samples.len() as f64 >= seconds * 16_000.0 {
                    break;
                }
                samples.push((f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).clamp(-1.0, 1.0) * 32767.0) as i16);
            }
            first.get_or_insert(start);
            last = end;
            covered = end;
            if samples.len() as f64 >= seconds * 16_000.0 {
                break;
            }
        }
        if !samples.is_empty() {
            let start = first.unwrap_or(0.0);
            let wav = crate::generation::probe::wav_pcm16(&samples, 16_000);
            let detail = json!({"display": display, "version_id": version, "start_s": start, "end_s": (start + samples.len() as f64 / 16_000.0).min(last)});
            let vault = ctx.lock()?;
            register_native(&vault, &grant.id, media, "audio", "audio/wav", &wav, &detail)?;
        }
    }
    Ok(())
}
