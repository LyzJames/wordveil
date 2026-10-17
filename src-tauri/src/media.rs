//! Fixed FFmpeg/ffprobe templates in a no-network AppContainer (design 9.13).
//!
//! Only core-built argument lists run: no user/model URL, device, filter graph
//! or protocol beyond `file`. Decoding needs a seekable plaintext copy of the
//! original; it lives in a per-task scratch directory that is ACL-protected and
//! removed on drop (and swept at start-up after a crash).
use crate::{
    Error, Result,
    isolation::{IsolatedProcess, SpawnPolicy},
};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

pub const SAMPLE_RATE: u32 = 16000;

pub struct MediaTools {
    pub bin: PathBuf,
    pub work_root: PathBuf,
}

/// Plaintext media workspace; removed when dropped.
pub struct Scratch {
    pub root: PathBuf,
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Clone)]
pub struct FramePick {
    pub target_s: f64,
    pub pts_s: f64,
}

impl MediaTools {
    pub fn scratch(&self) -> Result<Scratch> {
        let root = self.work_root.join(format!("media-{}", uuid::Uuid::new_v4().simple()));
        let input_dir = root.join("in");
        let output_dir = root.join("out");
        std::fs::create_dir_all(&input_dir)?;
        std::fs::create_dir_all(&output_dir)?;
        Ok(Scratch { root, input_dir, output_dir })
    }

    fn run(&self, tool: &str, arguments: Vec<String>, scratch: &Scratch, cancellation: &AtomicBool, timeout: Duration, capture: bool) -> Result<Vec<u8>> {
        let policy = SpawnPolicy {
            executable: self.bin.join(tool),
            arguments,
            read_only: vec![self.bin.clone(), scratch.input_dir.clone()],
            scratch: scratch.output_dir.clone(),
            memory_limit: 1024 * 1024 * 1024,
        };
        let deadline = Instant::now() + timeout;
        let mut process = IsolatedProcess::spawn(policy)?;
        let output = if capture { process.read_until_eof(cancellation, deadline, 32 * 1024 * 1024)? } else { process.read_until_eof(cancellation, deadline, 4 * 1024 * 1024)? };
        let code = process.wait_exit(cancellation, deadline)?;
        if code != 0 {
            return Err(Error::Worker(format!("{}_exit_{code}", tool.trim_end_matches(".exe"))));
        }
        Ok(output)
    }

    fn input_arg(path: &Path) -> Result<String> {
        let text = path.to_str().ok_or(Error::Contract)?;
        Ok(text.strip_prefix("\\\\?\\").unwrap_or(text).to_owned())
    }

    /// Container/stream metadata as ffprobe JSON.
    pub fn probe(&self, input: &Path, scratch: &Scratch, cancellation: &AtomicBool) -> Result<Value> {
        let arguments = ["-v", "error", "-protocol_whitelist", "file", "-print_format", "json", "-show_format", "-show_streams"]
            .iter()
            .map(|s| s.to_string())
            .chain([Self::input_arg(input)?])
            .collect();
        let bytes = self.run("ffprobe.exe", arguments, scratch, cancellation, Duration::from_secs(60), true)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Worker("ffprobe_output".into()))?;
        Ok(value)
    }

    /// Exact video packet timestamps (demux only, no decoding).
    pub fn video_timestamps(&self, input: &Path, scratch: &Scratch, cancellation: &AtomicBool) -> Result<Vec<f64>> {
        let arguments = ["-v", "error", "-protocol_whitelist", "file", "-select_streams", "v:0", "-show_entries", "packet=pts_time", "-of", "csv=p=0"]
            .iter()
            .map(|s| s.to_string())
            .chain([Self::input_arg(input)?])
            .collect();
        let bytes = self.run("ffprobe.exe", arguments, scratch, cancellation, Duration::from_secs(120), true)?;
        let mut times: Vec<f64> = String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|line| line.trim().trim_end_matches(',').parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .collect();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        times.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        Ok(times)
    }

    /// Decode a time range of the first audio stream to 16 kHz mono f32.
    pub fn decode_audio(&self, input: &Path, start_s: f64, duration_s: f64, scratch: &Scratch, cancellation: &AtomicBool) -> Result<Vec<f32>> {
        if !(start_s >= 0.0 && duration_s > 0.0 && duration_s <= 1320.0) {
            return Err(Error::Contract);
        }
        let output = scratch.output_dir.join("audio.f32");
        let arguments = vec![
            "-hide_banner".into(), "-nostdin".into(), "-loglevel".into(), "error".into(), "-y".into(),
            "-protocol_whitelist".into(), "file".into(),
            "-ss".into(), format!("{start_s:.3}"), "-t".into(), format!("{duration_s:.3}"),
            "-i".into(), Self::input_arg(input)?,
            "-map".into(), "0:a:0".into(), "-ac".into(), "1".into(), "-ar".into(), SAMPLE_RATE.to_string(),
            "-f".into(), "f32le".into(), Self::input_arg(&output)?,
        ];
        self.run("ffmpeg.exe", arguments, scratch, cancellation, Duration::from_secs(600), false)?;
        let bytes = std::fs::read(&output)?;
        let _ = std::fs::remove_file(&output);
        if bytes.len() % 4 != 0 {
            return Err(Error::Worker("audio_decode_misaligned".into()));
        }
        let samples: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        if samples.iter().any(|v| !v.is_finite()) {
            return Err(Error::Worker("audio_decode_non_finite".into()));
        }
        Ok(samples)
    }

    /// Decode one frame at an exact packet timestamp to BMP (no zlib/PNG in the
    /// LGPL build; the local worker converts BMP to PNG).
    pub fn frame_bmp(&self, input: &Path, pts_s: f64, scratch: &Scratch, cancellation: &AtomicBool) -> Result<Vec<u8>> {
        let output = scratch.output_dir.join(format!("frame-{}.bmp", uuid::Uuid::new_v4().simple()));
        let arguments = vec![
            "-hide_banner".into(), "-nostdin".into(), "-loglevel".into(), "error".into(), "-y".into(),
            "-protocol_whitelist".into(), "file".into(),
            "-ss".into(), format!("{pts_s:.6}"), "-i".into(), Self::input_arg(input)?,
            "-map".into(), "0:v:0".into(), "-frames:v".into(), "1".into(), "-c:v".into(), "bmp".into(),
            Self::input_arg(&output)?,
        ];
        self.run("ffmpeg.exe", arguments, scratch, cancellation, Duration::from_secs(120), false)?;
        let bytes = std::fs::read(&output)?;
        let _ = std::fs::remove_file(&output);
        if !bytes.starts_with(b"BM") {
            return Err(Error::Worker("frame_decode_failed".into()));
        }
        Ok(bytes)
    }
}

/// Uniform coverage sampling on real packet timestamps (design 6: no frame
/// number or average fps substitution). Returns at most `limit` picks.
pub fn pick_frames(timestamps: &[f64], duration_s: f64, limit: usize) -> Vec<FramePick> {
    if timestamps.is_empty() || limit == 0 || duration_s <= 0.0 {
        return vec![];
    }
    let count = limit.min(timestamps.len()).min(((duration_s / 2.0).ceil() as usize).max(1));
    let mut picks: Vec<FramePick> = vec![];
    for index in 0..count {
        let target = duration_s * (index as f64 + 0.5) / count as f64;
        let chosen = timestamps.iter().copied().find(|t| *t >= target).unwrap_or(*timestamps.last().unwrap_or(&0.0));
        if !picks.iter().any(|p| (p.pts_s - chosen).abs() < 1e-6) {
            picks.push(FramePick { target_s: target, pts_s: chosen });
        }
    }
    picks
}

/// 30 s windows with 1 s overlap (design 5.2 initial scheduling values).
pub fn audio_windows(total_samples: usize, window_s: f64, overlap_s: f64) -> Vec<(usize, usize)> {
    let window = (window_s * SAMPLE_RATE as f64) as usize;
    let step = window - (overlap_s * SAMPLE_RATE as f64) as usize;
    let mut output = vec![];
    let mut start = 0;
    while start < total_samples {
        let end = (start + window).min(total_samples);
        output.push((start, end));
        if end == total_samples {
            break;
        }
        start += step;
    }
    output
}
