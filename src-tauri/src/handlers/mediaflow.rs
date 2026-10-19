//! media-decode, audio-transcribe and image/audio/video review (design 9.13,
//! multimodal 2-6). Extraction (OCR, ASR, frames) is local. With an approved
//! grant, "analyze" runs the LangGraph media review flow: native observation
//! of the approved media where the connection passed the matching self-test,
//! a field comparison with the local text, and a verified written answer.
use super::*;
use crate::{
    media::{audio_windows, pick_frames, SAMPLE_RATE},
    vault::now_rfc3339,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::time::Duration;

const ASR_SOURCE_SECONDS: f64 = 1200.0;
const ASR_PROCESSED_SECONDS: f64 = 1320.0;
const ASR_SEGMENTS: f64 = 48.0;
const VISUAL_SAMPLES: usize = 24;

pub(crate) fn charge(ctx: &Ctx, root: &str, key: &str, amount: f64, limit: f64) -> Result<()> {
    let mut vault = ctx.lock()?;
    let tx = vault.runtime.ledger.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let used: f64 = tx.query_row("SELECT value FROM root_usage WHERE root=? AND key=?", params![root, key], |r| r.get(0)).optional()?.unwrap_or(0.0);
    if used + amount > limit + 1e-6 {
        return Err(Error::Budget);
    }
    tx.execute("INSERT INTO root_usage(root,key,value) VALUES(?,?,?) ON CONFLICT(root,key) DO UPDATE SET value=value+excluded.value", params![root, key, amount])?;
    tx.commit()?;
    Ok(())
}

fn used(ctx: &Ctx, root: &str, key: &str) -> Result<f64> {
    Ok(ctx.lock()?.db().query_row("SELECT value FROM root_usage WHERE root=? AND key=?", params![root, key], |r| r.get(0)).optional()?.unwrap_or(0.0))
}

fn technical_probe(probe: &Value) -> (Option<f64>, bool, bool, Value) {
    let duration = probe["format"]["duration"].as_str().and_then(|d| d.parse::<f64>().ok());
    let mut streams = vec![];
    let (mut audio, mut video) = (false, false);
    for stream in probe["streams"].as_array().cloned().unwrap_or_default() {
        let kind = stream["codec_type"].as_str().unwrap_or("");
        audio |= kind == "audio";
        video |= kind == "video";
        // Container tags (titles, comments) are dropped; only technical fields stay.
        streams.push(json!({"index": stream["index"], "codec_type": kind, "codec_name": stream["codec_name"], "time_base": stream["time_base"], "start_time": stream["start_time"],
            "duration": stream["duration"], "sample_rate": stream["sample_rate"], "channels": stream["channels"], "width": stream["width"], "height": stream["height"]}));
    }
    (duration, audio, video, json!({"format": probe["format"]["format_name"], "duration": duration, "streams": streams}))
}

pub fn decode(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "media.decode")?;
    ctx.authorize(invocation, "vault.write")?;
    let source = text_of(input, "source_ref")?;
    let range = text_of(input, "range_ref")?;
    let outputs: Vec<String> = input["outputs"].as_array().ok_or(Error::Contract)?.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let (version, blob, range_media, start, end): (String, String, String, f64, Option<f64>) = {
        let vault = ctx.lock()?;
        vault.expect_kind(&source, &["MediaVersionRef"])?;
        let (version, blob): (String, String) = vault.db().query_row("SELECT m.version_id,v.original_blob FROM media m JOIN document_versions v ON v.id=m.version_id WHERE m.id=?", [&source], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let (range_media, start, end): (String, f64, Option<f64>) = vault.db().query_row("SELECT media_id,start_s,end_s FROM ranges WHERE id=?", [&range], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.ok_or(Error::Denied)?;
        (version, blob, range_media, start, end)
    };
    if range_media != source {
        return Err(Error::Denied);
    }
    let scratch = ctx.services.media.scratch()?;
    let input_file = scratch.input_dir.join("input.media");
    {
        let vault = ctx.lock()?;
        let file = std::fs::File::create(&input_file)?;
        vault.copy_blob_to(&blob, std::io::BufWriter::new(file))?;
    }
    ctx.report(invocation, "probe", 0, 3);
    let probe = ctx.services.media.probe(&input_file, &scratch, ctx.cancel)?;
    let (duration, has_audio, has_video, summary) = technical_probe(&probe);
    let duration = duration.ok_or(Error::Worker("media_duration_unknown".into()))?;
    {
        let vault = ctx.lock()?;
        vault.db().execute("UPDATE media SET probe=?,duration_s=?,has_audio=?,has_video=? WHERE id=?", params![summary.to_string(), duration, i64::from(has_audio), i64::from(has_video), source])?;
    }
    let range_end = end.unwrap_or(duration).min(duration);
    let mut omitted = vec![];
    let mut parts = completed(None);
    let mut segments_out = vec![];
    let mut frames_out = vec![];
    if outputs.iter().any(|o| o == "audio") && has_audio && range_end > start {
        let remaining = ASR_SOURCE_SECONDS - used(ctx, invocation.root_id(), "asr_source_seconds")?;
        let length = (range_end - start).min(remaining.max(0.0));
        if length < range_end - start {
            omitted.push(json!({"kind": "audio", "start_s": start + length, "end_s": range_end, "reason": "root_asr_budget_1200s"}));
            parts.status = "partial".into();
        }
        if length > 0.0 {
            charge(ctx, invocation.root_id(), "asr_source_seconds", length, ASR_SOURCE_SECONDS)?;
            ctx.report(invocation, "decode_audio", 1, 3);
            let samples = ctx.services.media.decode_audio(&input_file, start, length, &scratch, ctx.cancel)?;
            let mut vault = ctx.lock()?;
            let stale: Vec<(String, String)> = {
                let mut statement = vault.db().prepare("SELECT id,pcm_blob FROM audio_segments WHERE media_id=?")?;
                let rows = statement.query_map([&source], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            if !stale.is_empty() {
                // Replaced segments take their transcripts and the version's chunks along.
                vault.db().execute("DELETE FROM blocks WHERE version_id=? AND source='asr'", [&version])?;
                super::indexing::drop_version_index(&mut vault, &version)?;
            }
            for (id, pcm) in stale {
                vault.db().execute("DELETE FROM audio_segments WHERE id=?", [&id])?;
                vault.db().execute("UPDATE objects SET active=0 WHERE id=?", [&id])?;
                vault.delete_blob(&pcm)?;
            }
            for (seq, (from, to)) in audio_windows(samples.len(), 30.0, 1.0).into_iter().enumerate() {
                let id = vault.register("AudioSegmentRef", "m1", Some(&source))?;
                let bytes: Vec<u8> = samples[from..to].iter().flat_map(|v| v.to_le_bytes()).collect();
                let pcm = vault.put_blob(&id, "audio_pcm_f32", &bytes)?;
                let (start_s, end_s) = (start + from as f64 / SAMPLE_RATE as f64, start + to as f64 / SAMPLE_RATE as f64);
                vault.db().execute(
                    "INSERT INTO audio_segments(id,media_id,seq,start_s,end_s,samples,pcm_blob,status,created_at) VALUES(?,?,?,?,?,?,?,'decoded',?)",
                    params![id, source, seq as i64, start_s, end_s, (to - from) as i64, pcm, now_rfc3339()],
                )?;
                segments_out.push(json!({"id": id, "seq": seq, "start_s": start_s, "end_s": end_s}));
            }
        }
    } else if outputs.iter().any(|o| o == "audio") && !has_audio {
        warn(&mut parts, "no_audio_stream", "该媒体没有可解码的音轨。");
    }
    if outputs.iter().any(|o| o == "frames") && has_video {
        {
            // Re-decoding replaces earlier samples (and their OCR) instead of reusing stale pages.
            let mut vault = ctx.lock()?;
            let old: Vec<(String, Option<String>)> = {
                let mut statement = vault.db().prepare("SELECT id,page_id FROM frames WHERE media_id=?")?;
                let rows = statement.query_map([&source], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            if !old.is_empty() {
                super::indexing::drop_version_index(&mut vault, &version)?;
            }
            for (frame, page) in old {
                if let Some(page) = page {
                    vault.db().execute("DELETE FROM blocks WHERE page_id=?", [&page])?;
                    let blobs: Vec<String> = {
                        let mut statement = vault.db().prepare("SELECT id FROM blobs WHERE owner=?")?;
                        let rows = statement.query_map([&page], |r| r.get(0))?;
                        rows.collect::<std::result::Result<_, _>>()?
                    };
                    for blob in blobs {
                        vault.delete_blob(&blob)?;
                    }
                    vault.db().execute("DELETE FROM pages WHERE id=?", [&page])?;
                    vault.db().execute("UPDATE objects SET active=0 WHERE id=?", [&page])?;
                }
                vault.db().execute("DELETE FROM frames WHERE id=?", [&frame])?;
                vault.db().execute("UPDATE objects SET active=0 WHERE id=?", [&frame])?;
            }
        }
        let available = VISUAL_SAMPLES.saturating_sub(used(ctx, invocation.root_id(), "visual_samples")? as usize);
        let timestamps: Vec<f64> = ctx.services.media.video_timestamps(&input_file, &scratch, ctx.cancel)?.into_iter().filter(|t| *t >= start && *t <= range_end).collect();
        let picks = pick_frames(&timestamps.iter().map(|t| t - start).collect::<Vec<_>>(), range_end - start, available);
        if !picks.is_empty() {
            charge(ctx, invocation.root_id(), "visual_samples", picks.len() as f64, VISUAL_SAMPLES as f64)?;
        }
        for (seq, pick) in picks.iter().enumerate() {
            ctx.check()?;
            ctx.report(invocation, "frames", seq as u64, picks.len() as u64);
            let pts = start + pick.pts_s;
            let bmp = ctx.services.media.frame_bmp(&input_file, pts, &scratch, ctx.cancel)?;
            let upload = ctx.services.local.upload(&bmp, ctx.cancel)?;
            let converted = ctx.services.local.call(&json!({"op": "image_pages", "upload_id": upload.id}), ctx.cancel, Duration::from_secs(120))?;
            drop(upload);
            let page = converted["pages"].get(0).ok_or(Error::Contract)?;
            let png = STANDARD.decode(page["png_base64"].as_str().ok_or(Error::Contract)?).map_err(|_| Error::Contract)?;
            let vault = ctx.lock()?;
            let page_no: i64 = 100_000 + seq as i64;
            let existing: Option<String> = vault.db().query_row("SELECT id FROM pages WHERE version_id=? AND page_no=?", params![version, page_no], |r| r.get(0)).optional()?;
            let page_id = match existing {
                Some(id) => id,
                None => {
                    let id = vault.register("PageRef", "m1", Some(&version))?;
                    let image = vault.put_blob(&id, "page_image", &png)?;
                    vault.db().execute(
                        "INSERT INTO pages(id,version_id,page_no,source,width,height,image_blob,created_at) VALUES(?,?,?,'frame',?,?,?,?)",
                        params![id, version, page_no, page["width"].as_f64(), page["height"].as_f64(), image, now_rfc3339()],
                    )?;
                    id
                }
            };
            let frame = vault.register("ImageRef", "m1", Some(&source))?;
            vault.db().execute("INSERT OR REPLACE INTO frames(id,media_id,seq,pts,time_s,page_id,created_at) VALUES(?,?,?,NULL,?,?,?)", params![frame, source, seq as i64, pts, page_id, now_rfc3339()])?;
            frames_out.push(json!({"frame_id": frame, "page_id": page_id, "seq": seq, "pts_s": pts, "target_s": start + pick.target_s}));
        }
        if timestamps.len() > picks.len() {
            parts.limitations.push(format!("视频按真实时间戳均匀抽取 {} 帧，未抽到的时段可能存在未检查的画面事件", picks.len()));
        }
    }
    drop(scratch);
    let payload = json!({"media_id": source, "version_id": version, "duration_s": duration, "probe": summary, "range": {"start_s": start, "end_s": range_end},
        "audio_segments": segments_out, "frames": frames_out,
        "coverage": {"audio_ranges": segments_out.iter().map(|s| json!([s["start_s"], s["end_s"]])).collect::<Vec<_>>(), "visual_samples": frames_out.len(), "omitted_ranges": omitted}});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "DecodedMediaBundleRef", &payload)?;
    parts.data_ref = Some(result);
    parts.processed = vec![source];
    Ok(parts)
}

/// Remove the duplicated words produced by the 1 s window overlap.
fn overlap_merge(previous: &str, next: &str) -> usize {
    let prev: Vec<char> = previous.chars().collect();
    let next_chars: Vec<char> = next.chars().collect();
    let max = prev.len().min(next_chars.len()).min(12);
    (2..=max).rev().find(|k| prev[prev.len() - k..] == next_chars[..*k]).unwrap_or(0)
}

pub fn transcribe(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "local.asr")?;
    ctx.authorize(invocation, "vault.write")?;
    let segments = refs_of(input, "audio_segments")?;
    if !input["language_hint"].is_null() || !input["vocabulary_ref"].is_null() {
        // No user-confirmed language hints or vocabularies exist in M1.
        return Err(Error::Denied);
    }
    if segments.is_empty() || segments.len() > ASR_SEGMENTS as usize {
        return Err(Error::Contract);
    }
    let mut rows: Vec<(String, String, i64, f64, f64, String)> = vec![];
    {
        let vault = ctx.lock()?;
        for id in &segments {
            let row: (String, i64, f64, f64, String) = vault.db().query_row("SELECT media_id,seq,start_s,end_s,pcm_blob FROM audio_segments WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?.ok_or(Error::Denied)?;
            rows.push((id.clone(), row.0, row.1, row.2, row.3, row.4));
        }
    }
    rows.sort_by_key(|r| (r.1.clone(), r.2));
    let processed: f64 = rows.iter().map(|r| r.4 - r.3).sum();
    charge(ctx, invocation.root_id(), "asr_processed_seconds", processed, ASR_PROCESSED_SECONDS)?;
    charge(ctx, invocation.root_id(), "asr_segments", rows.len() as f64, ASR_SEGMENTS)?;
    let mut results = vec![];
    for (position, (id, media, seq, start, end, pcm)) in rows.iter().enumerate() {
        ctx.check()?;
        ctx.report(invocation, "asr", position as u64, rows.len() as u64);
        let samples = ctx.lock()?.get_blob(pcm)?;
        let output = ctx.services.predict("asr", &json!({"kind": "asr", "samples_f32_base64": STANDARD.encode(&*samples), "sample_rate": 16000}), ctx.cancel)?;
        let text = output["transcription"].as_str().unwrap_or("").trim().to_owned();
        let truncated = output["truncated"].as_bool().unwrap_or(false);
        ctx.lock()?.db().execute(
            "UPDATE audio_segments SET transcript=?,raw_output=?,language=?,truncated=?,status='transcribed' WHERE id=?",
            params![text, output["raw_output"].as_str(), output["language"].as_str(), i64::from(truncated), id],
        )?;
        results.push((id.clone(), media.clone(), *seq, *start, *end, text, truncated, output["language"].as_str().map(str::to_owned)));
    }
    let mut blocks_by_version: std::collections::BTreeMap<String, Vec<(String, String, Value)>> = Default::default();
    let mut payload_segments = vec![];
    let mut previous: Option<(String, String)> = None;
    let mut truncated_any = false;
    for (id, media, seq, start, end, text, truncated, language) in &results {
        truncated_any |= *truncated;
        let skip = match &previous {
            Some((prev_media, prev_text)) if prev_media == media => overlap_merge(prev_text, text),
            _ => 0,
        };
        let merged: String = text.chars().skip(skip).collect();
        let version: String = ctx.lock()?.db().query_row("SELECT version_id FROM media WHERE id=?", [media], |r| r.get(0))?;
        if !merged.trim().is_empty() {
            blocks_by_version.entry(version).or_default().push((id.clone(), merged.clone(), json!({"type": "time", "media_id": media, "segment_id": id, "seq": seq, "start_s": start, "end_s": end, "truncated": truncated, "overlap_chars_removed": skip})));
        }
        payload_segments.push(json!({"id": id, "media_id": media, "seq": seq, "start_s": start, "end_s": end, "text": text, "merged_text": merged, "language": language, "truncated": truncated}));
        previous = Some((media.clone(), text.clone()));
    }
    {
        let mut vault = ctx.lock()?;
        let tx = vault.runtime.ledger.db.transaction()?;
        for (version, blocks) in &blocks_by_version {
            for (segment, _, _) in blocks {
                tx.execute("DELETE FROM blocks WHERE version_id=? AND source='asr' AND json_extract(locator,'$.segment_id')=?", params![version, segment])?;
            }
            let base: i64 = tx.query_row("SELECT COALESCE(MAX(ordinal),-1)+1 FROM blocks WHERE version_id=?", [version], |r| r.get(0))?;
            for (index, (_, text, locator)) in blocks.iter().enumerate() {
                tx.execute("INSERT INTO blocks(version_id,page_id,ordinal,kind,text,locator,confidence,source) VALUES(?,NULL,?,'transcript_segment',?,?,NULL,'asr')",
                    params![version, base + index as i64, text, locator.to_string()])?;
            }
        }
        tx.commit()?;
    }
    let payload = json!({"segments": payload_segments, "merge": "overlap-suffix-prefix/1 (raw text retained)", "word_timestamps": null,
        "limits": "分段时间区间可回放；无逐词对齐与说话人识别"});
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "TranscriptResultRef", &payload)?;
    let mut parts = completed(Some(result));
    parts.processed = segments;
    if truncated_any {
        parts.status = "partial".into();
        warn(&mut parts, "asr_truncated", "部分片段达到输出上限，转写可能不完整，请回放核对。");
    }
    Ok(parts)
}

fn question_exists(ctx: &Ctx, question: &str) -> Result<()> {
    ctx.lock()?.expect_kind(question, &["ImmutableQuestionRef"])
}

/// The media review flow for an approved "analyze" task; the core checks the
/// references it returns and builds the delivered answer.
#[allow(clippy::too_many_arguments)]
fn native_review(ctx: &Ctx, invocation: &Invocation, input: &Value, question: &str, versions: &[String], pages: &[String], frames: &[String], audio_source: Option<&str>, parts: &mut EnvelopeParts) -> Result<Value> {
    let root = invocation.root_id();
    let grant = super::answers::grant_for_root(ctx, root)?.ok_or(Error::Denied)?;
    super::observe::prepare_run_media(ctx, &grant, frames, audio_source)?;
    let grant = super::answers::grant_for_root(ctx, root)?.ok_or(Error::Denied)?;
    let (mut images, mut audio) = (vec![], vec![]);
    {
        let vault = ctx.lock()?;
        for id in &grant.media {
            let kind: Option<String> = vault.db().query_row("SELECT kind FROM native_media WHERE id=?", [id], |r| r.get(0)).optional()?;
            match kind.as_deref() {
                Some("image") if images.len() < super::observe::MAX_IMAGES_PER_REQUEST => images.push(id.clone()),
                Some("audio") if audio.is_empty() => audio.push(id.clone()),
                _ => {}
            }
        }
    }
    let bundle = super::observe::media_bundle(ctx, invocation, versions, pages)?;
    let text: String = ctx.lock()?.db().query_row("SELECT text FROM questions WHERE id=?", [question], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
    let outcome = super::flows::run(ctx, invocation, input, "media_review", || {
        Ok(json!({"owner": invocation.id(), "question_id": question, "question": text, "evidence_bundle": bundle, "images": images, "audio": audio}))
    })?;
    let out = &outcome.output;
    for (code, message) in outcome.warnings {
        if !parts.warnings.iter().any(|(c, _)| *c == code) {
            parts.warnings.push((code, message));
        }
    }
    for code in out["warnings"].as_array().cloned().unwrap_or_default().iter().filter_map(Value::as_str) {
        if let Some(message) = super::flows::flow_warning(code) {
            if !parts.warnings.iter().any(|(c, _)| c == code) {
                warn(parts, code, message);
            }
        }
    }
    let draft = super::flows::owned_result(ctx, &out["draft"], "DraftRef", root)?;
    let report = super::flows::owned_result(ctx, &out["report"], "ValidationReportRef", root)?;
    let answer = match (&draft, &report) {
        (Some((draft, _)), Some((report, _))) => super::workflows::deliver_answer(ctx, invocation, draft, report, out["revisions"].as_u64().unwrap_or(0).min(2), parts)?,
        _ => {
            parts.status = "partial".into();
            Value::Null
        }
    };
    let mut observations = vec![];
    for id in out["observations"].as_array().cloned().unwrap_or_default().into_iter().take(2) {
        if let Some((id, payload)) = super::flows::owned_result(ctx, &id, "MediaObservationRef", root)? {
            observations.push(json!({"observation_ref": id, "observations": payload["observation"]["observations"], "limitations": payload["observation"]["limitations"],
                "comparison": payload["comparison"], "media": payload["media"]}));
        }
    }
    let read = |kind: &str| observations.iter().any(|o| o["media"].as_array().is_some_and(|m| m.iter().any(|x| x["kind"] == kind)));
    let (visual, audible) = (read("image"), read("audio"));
    if !visual && !images.is_empty() || !audible && !audio.is_empty() {
        parts.status = "partial".into();
    }
    // The cited passages, read exactly, so the answer's citations open in place.
    let cited: Vec<String> = answer["labels"].as_array().cloned().unwrap_or_default().iter().filter_map(|l| l["evidence_id"].as_str().map(str::to_owned)).collect();
    let evidence_read = if cited.is_empty() {
        Value::Null
    } else {
        let (_, reading) = child(ctx, invocation, "evidence-read", json!({"evidence_ids": cited, "context_mode": "exact"}))?;
        data_of(&reading).map(Value::String).unwrap_or(Value::Null)
    };
    Ok(json!({"answer": answer, "evidence_read": evidence_read, "observations": observations, "visual_check": if visual { "performed" } else { "not_performed" },
        "direct_audio_check": if audible { "performed" } else { "not_performed" }, "challenge": super::workflows::challenge_summary(ctx, root, &out["challenge"])?,
        "process": super::flows::clean_process(&out["process"]), "flow_run": outcome.run,
        "usage": super::workflows::generation_usage(ctx, root)?, "connection": super::workflows::connection_summary(ctx, root)?}))
}

pub fn image_review(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let images = refs_of(input, "image_refs")?;
    let question = text_of(input, "question_id")?;
    let mode = text_of(input, "mode")?;
    question_exists(ctx, &question)?;
    let mut groups: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    {
        let vault = ctx.lock()?;
        for image in &images {
            let page = match vault.object_kind(image)?.as_str() {
                "PageRef" => image.clone(),
                "ImageRef" => vault.db().query_row("SELECT page_id FROM frames WHERE id=?", [image], |r| r.get::<_, String>(0))?,
                _ => return Err(Error::Denied),
            };
            let (version, status): (String, String) = vault.db().query_row("SELECT version_id,ocr_status FROM pages WHERE id=?", [&page], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if status != "done" {
                groups.entry(version).or_default().push(page);
            }
        }
    }
    let mut parts = completed(None);
    for (version, pages) in groups {
        let (_, envelope) = child(ctx, invocation, "document-ocr", json!({"document_version": version, "page_ids": pages, "purpose": "read"}))?;
        if status_of(&envelope) != "completed" {
            parts.status = "partial".into();
            parts.warnings.extend(child_warnings(&envelope));
        }
    }
    let mut pages_out = vec![];
    {
        let vault = ctx.lock()?;
        for image in &images {
            let page = match vault.object_kind(image)?.as_str() {
                "ImageRef" => vault.db().query_row("SELECT page_id FROM frames WHERE id=?", [image], |r| r.get::<_, String>(0))?,
                _ => image.clone(),
            };
            let mut statement = vault.db().prepare("SELECT text,confidence FROM blocks WHERE page_id=? AND source='ocr' ORDER BY ordinal")?;
            let lines: Vec<(String, Option<f64>)> = statement.query_map([&page], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
            pages_out.push(json!({"image_ref": image, "page_id": page, "text": lines.iter().map(|l| l.0.clone()).collect::<Vec<_>>().join("\n"), "lines": lines.len(),
                "low_confidence_lines": lines.iter().filter(|l| l.1.unwrap_or(1.0) < 0.8).count()}));
        }
    }
    let mut payload = json!({"mode": mode, "question_id": question, "pages": pages_out, "visual_check": "not_performed",
        "visual_check_reason": "原生图片复核需要通过视觉能力自检的生成接口与授权；以上为本机 OCR 文字，未作画面理解"});
    let analyze = mode == "analyze" && super::answers::generation_available(ctx, invocation)?;
    if analyze {
        let page_ids: Vec<String> = pages_out.iter().filter_map(|p| p["page_id"].as_str().map(str::to_owned)).collect();
        let mut versions: Vec<String> = vec![];
        for page in &page_ids {
            let version: String = ctx.lock()?.db().query_row("SELECT version_id FROM pages WHERE id=?", [page], |r| r.get(0))?;
            if !versions.contains(&version) {
                versions.push(version);
            }
        }
        let review = native_review(ctx, invocation, input, &question, &versions, &page_ids, &[], None, &mut parts)?;
        for (key, value) in review.as_object().cloned().unwrap_or_default() {
            payload[key] = value;
        }
    }
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ReviewResultRef", &payload)?;
    parts.data_ref = Some(result);
    parts.processed = images;
    if payload["visual_check"] != "performed" {
        parts.limitations.push("visual_check_not_performed: 仅本机 OCR 文字".into());
    }
    if mode == "analyze" && !analyze {
        parts.status = "requires_generation".into();
        warn(&mut parts, "requires_generation", "图片分析需要连接并授权生成接口；已交付本机 OCR 提取结果。");
    }
    Ok(parts)
}

pub fn audio_review(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let source = text_of(input, "source_ref")?;
    let range = text_of(input, "range_ref")?;
    let question = text_of(input, "question_id")?;
    let goal = text_of(input, "analysis_goal")?;
    question_exists(ctx, &question)?;
    let mut parts = completed(None);
    let bundle = match input["decoded_bundle_ref"].as_str() {
        Some(bundle) => {
            let (_, kind, payload) = ctx.lock()?.get_result(bundle)?;
            if kind != "DecodedMediaBundleRef" || payload["media_id"].as_str() != Some(source.as_str()) {
                return Err(Error::Denied);
            }
            payload
        }
        None => {
            let (_, envelope) = child(ctx, invocation, "media-decode", json!({"source_ref": source, "range_ref": range, "outputs": ["metadata", "audio"]}))?;
            if !matches!(status_of(&envelope), "completed" | "partial") {
                return Err(Error::Worker(child_warnings(&envelope).first().map(|w| crate::sanitize_code(&w.0.replace(':', "_"))).unwrap_or_else(|| "media_decode_failed".into())));
            }
            if status_of(&envelope) == "partial" {
                parts.status = "partial".into();
                parts.warnings.extend(child_warnings(&envelope));
            }
            ctx.lock()?.get_result(&data_of(&envelope).ok_or(Error::Integrity)?)?.2
        }
    };
    let segments: Vec<String> = bundle["audio_segments"].as_array().cloned().unwrap_or_default().iter().filter_map(|s| s["id"].as_str().map(str::to_owned)).collect();
    let transcript = if segments.is_empty() {
        warn(&mut parts, "no_audio_to_transcribe", "所选范围内没有可转写的音频。");
        Value::Null
    } else {
        let (_, envelope) = child(ctx, invocation, "audio-transcribe", json!({"audio_segments": segments}))?;
        if status_of(&envelope) != "completed" {
            parts.status = "partial".into();
            parts.warnings.extend(child_warnings(&envelope));
        }
        data_of(&envelope).map(Value::String).unwrap_or(Value::Null)
    };
    let mut payload = json!({"media_id": source, "question_id": question, "analysis_goal": goal, "transcript": transcript, "coverage": bundle["coverage"],
        "direct_audio_check": "not_performed", "direct_audio_reason": "原音频复核需要通过音频输入自检的生成接口与授权"});
    let analyze = goal == "analyze" && super::answers::generation_available(ctx, invocation)?;
    if analyze {
        // The transcript becomes locatable evidence before the written analysis.
        let version = bundle["version_id"].as_str().ok_or(Error::Integrity)?.to_owned();
        let (_, index) = child(ctx, invocation, "knowledge-index", json!({"document_versions": [version], "operation": "build"}))?;
        if status_of(&index) != "completed" {
            parts.status = "partial".into();
            parts.warnings.extend(child_warnings(&index));
        }
        let review = native_review(ctx, invocation, input, &question, std::slice::from_ref(&version), &[], &[], Some(&source), &mut parts)?;
        for (key, value) in review.as_object().cloned().unwrap_or_default() {
            payload[key] = value;
        }
    }
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ReviewResultRef", &payload)?;
    parts.data_ref = Some(result);
    if goal == "analyze" && !analyze {
        parts.status = "requires_generation".into();
        warn(&mut parts, "requires_generation", "音频分析需要连接并授权生成接口；已交付本机 ASR 转写。");
    }
    Ok(parts)
}

pub fn video_review(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "vault.read")?;
    let source = text_of(input, "source_ref")?;
    let range = text_of(input, "range_ref")?;
    let question = text_of(input, "question_id")?;
    let goal = text_of(input, "analysis_goal")?;
    question_exists(ctx, &question)?;
    let mut parts = completed(None);
    let (_, decoded) = child(ctx, invocation, "media-decode", json!({"source_ref": source, "range_ref": range, "outputs": ["metadata", "audio", "frames"]}))?;
    if !matches!(status_of(&decoded), "completed" | "partial") {
        return Err(Error::Worker("media_decode_failed".into()));
    }
    let bundle_ref = data_of(&decoded).ok_or(Error::Integrity)?;
    let bundle = ctx.lock()?.get_result(&bundle_ref)?.2;
    let frames: Vec<String> = bundle["frames"].as_array().cloned().unwrap_or_default().iter().filter_map(|f| f["page_id"].as_str().map(str::to_owned)).collect();
    let mut ocr_ref = Value::Null;
    if !frames.is_empty() {
        let (_, envelope) = child(ctx, invocation, "image-review", json!({"image_refs": frames, "question_id": question, "mode": "extract_text"}))?;
        if status_of(&envelope) != "completed" {
            parts.status = "partial".into();
            parts.warnings.extend(child_warnings(&envelope));
        }
        ocr_ref = data_of(&envelope).map(Value::String).unwrap_or(Value::Null);
    }
    let (_, audio) = child(ctx, invocation, "audio-review", json!({"source_ref": source, "range_ref": range, "question_id": question, "decoded_bundle_ref": bundle_ref, "analysis_goal": "text_only"}))?;
    if status_of(&audio) != "completed" {
        parts.status = "partial".into();
        parts.warnings.extend(child_warnings(&audio));
    }
    let version = bundle["version_id"].as_str().ok_or(Error::Integrity)?.to_owned();
    let (_, index) = child(ctx, invocation, "knowledge-index", json!({"document_versions": [version], "operation": "build"}))?;
    if status_of(&index) != "completed" {
        parts.status = "partial".into();
        parts.warnings.extend(child_warnings(&index));
    }
    let mut payload = json!({"media_id": source, "question_id": question, "analysis_goal": goal, "frame_ocr": ocr_ref, "audio_review": data_of(&audio),
        "coverage": {"visual_samples": bundle["coverage"]["visual_samples"], "ocr_ranges": bundle["frames"], "asr_ranges": bundle["coverage"]["audio_ranges"],
            "direct_audio_ranges": [], "native_video_ranges": [], "omitted_ranges": bundle["coverage"]["omitted_ranges"]},
        "limits": "抽帧 OCR 只覆盖实际样本画面中的文字；ASR 只覆盖语音文字；原生视频与联合媒体输入不在本版本"});
    let analyze = goal == "analyze" && super::answers::generation_available(ctx, invocation)?;
    if analyze {
        let review = native_review(ctx, invocation, input, &question, std::slice::from_ref(&version), &[], &frames, Some(&source), &mut parts)?;
        if review["visual_check"] == "performed" {
            payload["limits"] = json!("模型直接读取了获准的抽帧图片（不是完整视频）；ASR 只覆盖语音文字；原生视频与联合媒体输入不在本版本");
        }
        for (key, value) in review.as_object().cloned().unwrap_or_default() {
            payload[key] = value;
        }
    }
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ReviewResultRef", &payload)?;
    parts.data_ref = Some(result);
    if goal == "analyze" && !analyze {
        parts.status = "requires_generation".into();
        warn(&mut parts, "requires_generation", "视频分析需要连接并授权生成接口；已交付抽帧文字与音轨转写。");
    }
    Ok(parts)
}
