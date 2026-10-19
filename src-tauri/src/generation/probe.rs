//! Capability self-test with synthetic samples only (design 8.4).
//!
//! Each capability is checked by one bounded request against the exact
//! endpoint the user configured; nothing from the vault is sent. Results are
//! supported / unsupported / unknown / unavailable: authentication, balance,
//! rate-limit, timeout and network failures are `unavailable` (temporary), not
//! a permanent "unsupported". Native audio, video and joint media are not
//! exercised in this version and stay `unknown`.
use super::{GenRequest, GenResponse, Message, OutputMode, Part, Profile, Protocol, ToolSpec, adapters};
use crate::Error;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use zeroize::Zeroizing;

pub const MAX_PROBE_REQUESTS: u32 = 7;

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProbeResult {
    pub capability: &'static str,
    pub status: &'static str,
    pub evidence: String,
    pub limits: Value,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub sent: bool,
}

const SYSTEM: &str = "这是律金智库的接口能力自检，内容均为合成测试样本。严格按要求作答，不要添加解释。";

fn text_request(prompt: &str, output: OutputMode, stream: bool) -> GenRequest {
    GenRequest { system: SYSTEM.into(), messages: vec![Message { role: "user", parts: vec![Part::Text(prompt.into())] }], max_output_tokens: 256, output, tools: vec![], stream }
}

fn temporary(code: &str) -> bool {
    matches!(code, "auth_failed" | "insufficient_balance" | "rate_limited" | "timeout" | "network" | "connection_failed" | "host_not_found" | "tls" | "server_error" | "server_busy" | "forbidden" | "model_not_found" | "redirect_refused")
}

fn failure(capability: &'static str, error: &Error) -> ProbeResult {
    let code = match error {
        Error::Remote(code) => code.clone(),
        other => other.code(),
    };
    let (status, evidence) = if temporary(&code) {
        ("unavailable", format!("暂不可用：{}", describe(&code)))
    } else if code == "bad_request" {
        ("unsupported", "接口拒绝了该能力所需的请求字段（bad_request）".to_owned())
    } else {
        ("unknown", format!("未能判定：{}", describe(&code)))
    };
    ProbeResult { capability, status, evidence, limits: Value::Null, input_tokens: 0, output_tokens: 0, sent: !matches!(error, Error::Cancelled) }
}

/// Stable codes to short Chinese descriptions shown in the settings view.
pub fn describe(code: &str) -> &'static str {
    match code {
        "auth_failed" => "认证失败（请检查 API Key）",
        "insufficient_balance" => "账户余额不足",
        "rate_limited" => "请求过于频繁（限流）",
        "timeout" => "请求超时",
        "network" | "connection_failed" => "网络连接失败",
        "host_not_found" => "无法解析服务地址",
        "tls" => "TLS 证书或加密连接失败",
        "server_error" => "服务端错误",
        "server_busy" => "服务繁忙",
        "forbidden" => "无权使用该资源或地区受限",
        "model_not_found" => "模型 ID 不存在或无权使用",
        "redirect_refused" => "服务返回了重定向，已拒绝跟随",
        "bad_request" => "请求格式或参数不被接受",
        "context_too_long" => "内容超过模型上下文长度",
        "refused" => "模型拒绝回答",
        "malformed_response" | "malformed_stream" => "响应格式无法解析",
        "stream_incomplete" => "流式响应中途中断",
        "response_too_large" => "响应过大",
        "cancelled" => "已取消",
        _ => "其他错误",
    }
}

fn ok(capability: &'static str, response: &GenResponse, evidence: String, limits: Value) -> ProbeResult {
    ProbeResult { capability, status: "supported", evidence, limits, input_tokens: response.usage.input_tokens, output_tokens: response.usage.output_tokens, sent: true }
}

fn wrong(capability: &'static str, response: &GenResponse, evidence: String) -> ProbeResult {
    ProbeResult { capability, status: "unknown", evidence, limits: Value::Null, input_tokens: response.usage.input_tokens, output_tokens: response.usage.output_tokens, sent: true }
}

fn has_42(text: &str) -> bool {
    text.chars().filter(|c| !c.is_whitespace()).collect::<String>().contains("42")
}

fn parse_json(text: &str) -> Option<Value> {
    let trimmed = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    serde_json::from_str(trimmed).ok()
}

/// Run the self-test. `send` performs one real request and is supplied by the
/// caller so the request is recorded in the audit trail and counted.
pub fn run(profile: &Profile, key: Option<&Zeroizing<String>>, cancel: &AtomicBool, mut record: impl FnMut(&ProbeResult)) -> Vec<ProbeResult> {
    let mut results = vec![];
    let mut push = |result: ProbeResult, results: &mut Vec<ProbeResult>| {
        record(&result);
        results.push(result);
    };
    let mut ignore = |_: &str| {};
    // 1. Plain text: the baseline; if it is unavailable the rest is not tried.
    let text = adapters::call(profile, key, &text_request("17 加 25 等于多少？只回复阿拉伯数字。", OutputMode::Text, false), cancel, &mut ignore);
    let baseline = match &text {
        Ok(response) if has_42(&response.text) => {
            push(ok("text", response, "合成算术题返回了正确的纯文本".into(), json!({"max_output_tokens_requested": 256})), &mut results);
            true
        }
        Ok(response) => {
            push(wrong("text", response, "接口有响应，但内容与预期不符".into()), &mut results);
            true
        }
        Err(error) => {
            push(failure("text", error), &mut results);
            false
        }
    };
    if !baseline {
        for capability in ["streaming", "tool_calling", "json_object", "strict_schema", "vision", "audio_input"] {
            push(ProbeResult { capability, status: "unknown", evidence: "文本请求未成功，未继续检测".into(), limits: Value::Null, input_tokens: 0, output_tokens: 0, sent: false }, &mut results);
        }
    } else {
        // 2. Streaming.
        match adapters::call(profile, key, &text_request("17 加 25 等于多少？只回复阿拉伯数字。", OutputMode::Text, true), cancel, &mut ignore) {
            Ok(response) if response.stream_deltas > 0 && has_42(&response.text) => {
                let evidence = format!("收到 {} 段增量，内容完整{}", response.stream_deltas, if response.usage.output_tokens > 0 { "，含用量" } else { "，未返回用量" });
                push(ok("streaming", &response, evidence, json!({"usage_in_stream": response.usage.output_tokens > 0})), &mut results)
            }
            Ok(response) => push(wrong("streaming", &response, "流式响应未包含预期内容".into()), &mut results),
            Err(error) => push(failure("streaming", &error), &mut results),
        }
        // 3. Tool calling.
        let tool = ToolSpec { name: "record_sum".into(), description: "记录两个整数的加法。".into(), parameters: json!({"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}}, "required": ["a", "b"], "additionalProperties": false}) };
        let mut request = text_request("请调用 record_sum 工具，参数 a 为 17，b 为 25。", OutputMode::Text, false);
        request.tools = vec![tool];
        match adapters::call(profile, key, &request, cancel, &mut ignore) {
            Ok(response) => match response.tool_calls.iter().find(|c| c.name == "record_sum").and_then(|c| serde_json::from_str::<Value>(&c.arguments).ok()) {
                Some(arguments) if arguments["a"] == json!(17) && arguments["b"] == json!(25) => push(ok("tool_calling", &response, "返回了参数正确的工具调用".into(), Value::Null), &mut results),
                Some(_) => push(wrong("tool_calling", &response, "返回了工具调用，但参数不符".into()), &mut results),
                None => push(wrong("tool_calling", &response, "未返回工具调用（返回了文本）".into()), &mut results),
            },
            Err(error) => push(failure("tool_calling", &error), &mut results),
        }
        // 4. JSON object mode (syntax only). The Messages API has no such mode.
        if profile.protocol == Protocol::AnthropicMessages {
            push(ProbeResult { capability: "json_object", status: "unsupported", evidence: "该协议没有 JSON 对象模式；结构化输出改用强制工具输入".into(), limits: Value::Null, input_tokens: 0, output_tokens: 0, sent: false }, &mut results);
        } else {
            match adapters::call(profile, key, &text_request("以 JSON 对象返回 17 加 25 的结果，格式为 {\"sum\": 数字}。只输出 JSON。", OutputMode::JsonObject, false), cancel, &mut ignore) {
                Ok(response) => match parse_json(&response.text) {
                    Some(value) if value["sum"] == json!(42) => push(ok("json_object", &response, "返回了可解析的 JSON 对象".into(), Value::Null), &mut results),
                    _ => push(wrong("json_object", &response, "返回内容不是预期的 JSON".into()), &mut results),
                },
                Err(error) => push(failure("json_object", &error), &mut results),
            }
        }
        // 5. Strict schema.
        let schema = json!({"type": "object", "properties": {"sum": {"type": "integer"}, "unit": {"type": "string", "enum": ["个"]}}, "required": ["sum", "unit"], "additionalProperties": false});
        let request = text_request("计算 17 加 25，unit 填“个”。", OutputMode::JsonSchema { name: "sum_result".into(), schema: schema.clone() }, false);
        match adapters::call(profile, key, &request, cancel, &mut ignore) {
            Ok(response) => match parse_json(&response.text) {
                Some(value) if value["sum"] == json!(42) && value["unit"] == json!("个") && value.as_object().is_some_and(|o| o.len() == 2) => {
                    push(ok("strict_schema", &response, "按给定 JSON Schema 返回了严格结构".into(), Value::Null), &mut results)
                }
                _ => push(wrong("strict_schema", &response, "接受了 Schema 参数，但返回结构不符".into()), &mut results),
            },
            Err(error) => push(failure("strict_schema", &error), &mut results),
        }
        // 6. Vision: a generated image with three red squares on white.
        let request = GenRequest {
            system: SYSTEM.into(),
            messages: vec![Message { role: "user", parts: vec![Part::Text("图片里有几个红色正方形？只回复阿拉伯数字。".into()), Part::ImagePng(synthetic_png_data_url())] }],
            max_output_tokens: 64,
            output: OutputMode::Text,
            tools: vec![],
            stream: false,
        };
        match adapters::call(profile, key, &request, cancel, &mut ignore) {
            Ok(response) if response.text.contains('3') || response.text.contains('三') => push(ok("vision", &response, "正确识别了合成图片中的红色方块数量".into(), json!({"tested_image": "PNG 192x96"})), &mut results),
            Ok(response) => push(wrong("vision", &response, "接受了图片，但回答与图片内容不符".into()), &mut results),
            Err(error) => push(failure("vision", &error), &mut results),
        }
        // 7. Audio input: a synthetic clip with three short tones.
        if profile.protocol == Protocol::AnthropicMessages {
            push(ProbeResult { capability: "audio_input", status: "unsupported", evidence: "该协议没有音频输入；音频按本机转写处理".into(), limits: Value::Null, input_tokens: 0, output_tokens: 0, sent: false }, &mut results);
        } else {
            let request = GenRequest {
                system: SYSTEM.into(),
                messages: vec![Message { role: "user", parts: vec![Part::Text("这段音频里有几声短促的提示音？只回复阿拉伯数字。".into()), Part::AudioWav(synthetic_wav_base64())] }],
                max_output_tokens: 64,
                output: OutputMode::Text,
                tools: vec![],
                stream: false,
            };
            match adapters::call(profile, key, &request, cancel, &mut ignore) {
                Ok(response) if response.text.contains('3') || response.text.contains('三') => push(ok("audio_input", &response, "正确数出了合成音频中的提示音".into(), json!({"tested_audio": "WAV 16 kHz 单声道 约 1.6 秒"})), &mut results),
                Ok(response) => push(wrong("audio_input", &response, "接受了音频，但回答与音频内容不符".into()), &mut results),
                Err(error) => push(failure("audio_input", &error), &mut results),
            }
        }
    }
    for capability in ["native_video", "joint_media"] {
        push(ProbeResult { capability, status: "unsupported", evidence: "本版本的适配器不发送原生视频或联合媒体输入；视频按抽帧图片与音轨分别处理".into(), limits: Value::Null, input_tokens: 0, output_tokens: 0, sent: false }, &mut results);
    }
    results
}

/// 16 kHz mono 16-bit WAV: three 0.15 s tones of 880 Hz with pauses.
pub fn synthetic_wav_base64() -> String {
    use base64::Engine;
    let rate = 16_000u32;
    let mut samples: Vec<i16> = vec![0; (rate as f32 * 0.2) as usize];
    for _ in 0..3 {
        for n in 0..(rate as f32 * 0.15) as usize {
            let t = n as f32 / rate as f32;
            samples.push(((t * 880.0 * std::f32::consts::TAU).sin() * 0.4 * i16::MAX as f32) as i16);
        }
        samples.extend(std::iter::repeat_n(0i16, (rate as f32 * 0.3) as usize));
    }
    base64::engine::general_purpose::STANDARD.encode(wav_pcm16(&samples, rate))
}

/// A canonical PCM16 mono WAV container.
pub fn wav_pcm16(samples: &[i16], rate: u32) -> Vec<u8> {
    let data = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// A 192x96 PNG: white background with three red 24x24 squares. Built with
/// stored (uncompressed) deflate blocks, so no image library is needed.
pub fn synthetic_png_data_url() -> String {
    use base64::Engine;
    let (width, height) = (192u32, 96u32);
    let mut raw = Vec::with_capacity(((width * 3 + 1) * height) as usize);
    for y in 0..height {
        raw.push(0u8);
        for x in 0..width {
            let red = (36..60).contains(&y) && [(24u32, 48u32), (84, 108), (144, 168)].iter().any(|(a, b)| (*a..*b).contains(&x));
            raw.extend_from_slice(if red { &[220, 20, 30] } else { &[255, 255, 255] });
        }
    }
    let mut zlib = vec![0x78, 0x01];
    for (index, chunk) in raw.chunks(65_535).enumerate() {
        let last = (index + 1) * 65_535 >= raw.len();
        zlib.push(last as u8);
        let length = chunk.len() as u16;
        zlib.extend_from_slice(&length.to_le_bytes());
        zlib.extend_from_slice(&(!length).to_le_bytes());
        zlib.extend_from_slice(chunk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + *byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let chunk = |kind: &[u8; 4], data: &[u8], png: &mut Vec<u8>| {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc_input = kind.to_vec();
        crc_input.extend_from_slice(data);
        png.extend_from_slice(&crc_input);
        png.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    };
    let mut header = vec![];
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &header, &mut png);
    chunk(b"IDAT", &zlib, &mut png);
    chunk(b"IEND", &[], &mut png);
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}
