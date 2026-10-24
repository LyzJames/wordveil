//! Protocol adapters: OpenAI-compatible Chat Completions, OpenAI Responses and
//! Anthropic Messages (design 8.3). Each adapter sends only the fields its
//! protocol defines; a different base_url is not assumed to support the same
//! features. Responses are normalised into `GenResponse`; provider error
//! bodies are reduced to stable codes and never logged.
use super::{GenRequest, GenResponse, OutputMode, Part, Profile, Protocol, Provider, ToolCall, Usage, transport};
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use zeroize::Zeroizing;

const RESPONSE_LIMIT: usize = 8 << 20;
const MAX_STREAM_BLOCKS: usize = 128;

fn stream_index(value: &Value) -> Result<usize> {
    value.as_u64().and_then(|v| usize::try_from(v).ok()).filter(|v| *v < MAX_STREAM_BLOCKS)
        .ok_or_else(|| Error::Remote("malformed_stream".into()))
}

/// Send one request. `on_delta` receives streamed text fragments (used only to
/// report progress; the final text is the concatenation).
pub fn call(profile: &Profile, key: Option<&Zeroizing<String>>, request: &GenRequest, cancel: &AtomicBool, on_delta: &mut dyn FnMut(&str)) -> Result<GenResponse> {
    if cancel.load(Ordering::Acquire) { return Err(Error::Cancelled); }
    // The connection's own output cap bounds every request (cost control).
    let capped;
    let request = if request.max_output_tokens > profile.max_output_tokens {
        capped = GenRequest { max_output_tokens: profile.max_output_tokens, ..request.clone() };
        &capped
    } else {
        request
    };
    let body = match profile.protocol {
        Protocol::ChatCompletions => chat_body(profile, request),
        Protocol::Responses => responses_body(profile, request),
        Protocol::AnthropicMessages => anthropic_body(profile, request),
    };
    let body = serde_json::to_vec(&body)?;
    let bearer;
    let mut headers: Vec<(&str, &str)> = vec![];
    match profile.protocol {
        Protocol::AnthropicMessages => {
            if let Some(key) = key {
                headers.push(("x-api-key", key.as_str()));
            }
            headers.push(("anthropic-version", "2023-06-01"));
        }
        _ => {
            if let Some(key) = key {
                bearer = Zeroizing::new(format!("Bearer {}", key.as_str()));
                headers.push(("Authorization", bearer.as_str()));
            }
        }
    }
    let operation = match profile.protocol {
        Protocol::ChatCompletions => "/chat/completions",
        Protocol::Responses => "/responses",
        Protocol::AnthropicMessages => "/messages",
    };
    let response = transport::post_json(&profile.endpoint, operation, &headers, &body, &profile.http(), request.stream)?;
    if !(200..300).contains(&response.status) {
        let status = response.status;
        let detail = response.read_all(64 * 1024).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        return Err(Error::Remote(refine_error(status, detail.as_ref()).into()));
    }
    let mut output = if request.stream {
        let reader = response.into_reader();
        match profile.protocol {
            Protocol::ChatCompletions => chat_stream(reader, cancel, on_delta)?,
            Protocol::Responses => responses_stream(reader, cancel, on_delta)?,
            Protocol::AnthropicMessages => anthropic_stream(reader, cancel, on_delta)?,
        }
    } else {
        let bytes = response.read_all(RESPONSE_LIMIT)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Remote("malformed_response".into()))?;
        match profile.protocol {
            Protocol::ChatCompletions => chat_parse(&value)?,
            Protocol::Responses => responses_parse(&value)?,
            Protocol::AnthropicMessages => anthropic_parse(&value)?,
        }
    };
    // Anthropic structured output arrives as the forced tool's input.
    if profile.protocol == Protocol::AnthropicMessages && output.finish == "tool_calls" {
        if let OutputMode::JsonSchema { name, .. } = &request.output {
            if let Some(call) = output.tool_calls.iter().find(|c| &c.name == name) {
                output.text = call.arguments.clone();
                output.finish = "stop".into();
            }
        }
    }
    if output.finish == "content_filter" {
        return Err(Error::Remote("refused".into()));
    }
    if output.finish == "other" {
        return Err(Error::Remote("generation_incomplete".into()));
    }
    Ok(output)
}

/// Provider error bodies are untrusted text; only known codes are kept.
fn refine_error(status: u16, body: Option<&Value>) -> &'static str {
    let code = body
        .map(|b| b.get("error").unwrap_or(b))
        .and_then(|e| e.get("code").or_else(|| e.get("type")))
        .and_then(Value::as_str)
        .unwrap_or("");
    match code {
        "context_length_exceeded" | "request_too_large" => "context_too_long",
        "insufficient_quota" | "billing_hard_limit_reached" => "insufficient_balance",
        "model_not_found" | "not_found_error" => "model_not_found",
        "invalid_api_key" | "authentication_error" => "auth_failed",
        "permission_error" => "forbidden",
        "rate_limit_error" | "rate_limit_exceeded" => "rate_limited",
        "overloaded_error" => "server_busy",
        _ if status == 404 => "model_not_found",
        _ if status == 503 => "server_busy",
        _ => transport::status_code(status),
    }
}

fn text_of(parts: &[Part]) -> Option<String> {
    match parts {
        [Part::Text(text)] => Some(text.clone()),
        _ => None,
    }
}

// ----------------------------------------------------------- Chat Completions

fn chat_body(profile: &Profile, request: &GenRequest) -> Value {
    let mut messages = vec![json!({"role": "system", "content": request.system})];
    for message in &request.messages {
        let content = match text_of(&message.parts) {
            Some(text) => Value::String(text),
            None => Value::Array(
                message
                    .parts
                    .iter()
                    .map(|part| match part {
                        Part::Text(text) => json!({"type": "text", "text": text}),
                        Part::ImagePng(url) => json!({"type": "image_url", "image_url": {"url": url}}),
                        Part::AudioWav(data) => json!({"type": "input_audio", "input_audio": {"data": data, "format": "wav"}}),
                    })
                    .collect(),
            ),
        };
        messages.push(json!({"role": message.role, "content": content}));
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(profile.model_id));
    body.insert("messages".into(), Value::Array(messages));
    // OpenAI's current models take max_completion_tokens; compatible servers and
    // DeepSeek use max_tokens.
    let limit_field = if profile.provider == Provider::Openai { "max_completion_tokens" } else { "max_tokens" };
    body.insert(limit_field.into(), json!(request.max_output_tokens));
    body.insert("stream".into(), json!(request.stream));
    if request.stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if profile.provider == Provider::Deepseek {
        // Thinking is on by default for DeepSeek; structured planning and
        // drafting use the non-thinking mode for bounded latency and cost.
        body.insert("thinking".into(), json!({"type": "disabled"}));
    }
    match &request.output {
        OutputMode::Text => {}
        OutputMode::JsonObject => {
            body.insert("response_format".into(), json!({"type": "json_object"}));
        }
        OutputMode::JsonSchema { name, schema } => {
            body.insert("response_format".into(), json!({"type": "json_schema", "json_schema": {"name": name, "schema": schema, "strict": true}}));
        }
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    Value::Object(body)
}

fn chat_finish(reason: Option<&str>) -> String {
    match reason {
        Some("stop") => "stop",
        Some("length") => "length",
        Some("tool_calls") | Some("function_call") => "tool_calls",
        Some("content_filter") => "content_filter",
        _ => "other",
    }
    .into()
}

fn chat_parse(value: &Value) -> Result<GenResponse> {
    let choice = value["choices"].get(0).ok_or(Error::Remote("malformed_response".into()))?;
    let message = &choice["message"];
    let tool_calls = message["tool_calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| Some(ToolCall { name: c["function"]["name"].as_str()?.to_owned(), arguments: c["function"]["arguments"].as_str().unwrap_or("").to_owned() }))
                .collect()
        })
        .unwrap_or_default();
    Ok(GenResponse {
        text: message["content"].as_str().unwrap_or("").to_owned(),
        tool_calls,
        finish: chat_finish(choice["finish_reason"].as_str()),
        usage: Usage { input_tokens: value["usage"]["prompt_tokens"].as_u64().unwrap_or(0), output_tokens: value["usage"]["completion_tokens"].as_u64().unwrap_or(0) },
        model: value["model"].as_str().map(str::to_owned),
        stream_deltas: 0,
    })
}

fn chat_stream(reader: Box<dyn std::io::Read + Send>, cancel: &AtomicBool, on_delta: &mut dyn FnMut(&str)) -> Result<GenResponse> {
    let mut out = GenResponse { finish: "other".into(), ..Default::default() };
    let mut calls: Vec<(String, String)> = vec![];
    let mut done = false;
    transport::read_sse(reader, cancel, |event| {
        if event.data.trim() == "[DONE]" {
            done = true;
            return Ok(false);
        }
        let chunk: Value = serde_json::from_str(&event.data).map_err(|_| Error::Remote("malformed_stream".into()))?;
        if chunk.get("error").is_some() {
            return Err(Error::Remote(refine_error(500, Some(&chunk)).into()));
        }
        if let Some(model) = chunk["model"].as_str() {
            out.model = Some(model.to_owned());
        }
        if let Some(choice) = chunk["choices"].get(0) {
            if let Some(text) = choice["delta"]["content"].as_str() {
                if !text.is_empty() {
                    out.text.push_str(text);
                    out.stream_deltas += 1;
                    on_delta(text);
                }
            }
            if let Some(fragments) = choice["delta"]["tool_calls"].as_array() {
                for fragment in fragments {
                    let index = stream_index(&fragment["index"])?;
                    while calls.len() <= index {
                        calls.push((String::new(), String::new()));
                    }
                    if let Some(name) = fragment["function"]["name"].as_str() {
                        calls[index].0.push_str(name);
                    }
                    if let Some(arguments) = fragment["function"]["arguments"].as_str() {
                        calls[index].1.push_str(arguments);
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                out.finish = chat_finish(Some(reason));
            }
        }
        if let Some(usage) = chunk["usage"].as_object() {
            out.usage = Usage { input_tokens: usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0), output_tokens: usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0) };
        }
        Ok(true)
    })?;
    if !done {
        return Err(Error::Remote("stream_incomplete".into()));
    }
    out.tool_calls = calls.into_iter().filter(|(name, _)| !name.is_empty()).map(|(name, arguments)| ToolCall { name, arguments }).collect();
    Ok(out)
}

// ------------------------------------------------------------------ Responses

fn responses_body(profile: &Profile, request: &GenRequest) -> Value {
    let input: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            let text_type = if message.role == "assistant" { "output_text" } else { "input_text" };
            let content: Vec<Value> = message
                .parts
                .iter()
                .map(|part| match part {
                    Part::Text(text) => json!({"type": text_type, "text": text}),
                    Part::ImagePng(url) => json!({"type": "input_image", "image_url": url}),
                    Part::AudioWav(data) => json!({"type": "input_audio", "input_audio": {"data": data, "format": "wav"}}),
                })
                .collect();
            json!({"role": message.role, "content": content})
        })
        .collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(profile.model_id));
    body.insert("instructions".into(), json!(request.system));
    body.insert("input".into(), Value::Array(input));
    body.insert("max_output_tokens".into(), json!(request.max_output_tokens));
    body.insert("stream".into(), json!(request.stream));
    // Ask the provider not to store this response for later retrieval. This is
    // a request parameter, not a guarantee about the provider's retention.
    body.insert("store".into(), json!(false));
    match &request.output {
        OutputMode::Text => {}
        OutputMode::JsonObject => {
            body.insert("text".into(), json!({"format": {"type": "json_object"}}));
        }
        OutputMode::JsonSchema { name, schema } => {
            body.insert("text".into(), json!({"format": {"type": "json_schema", "name": name, "schema": schema, "strict": true}}));
        }
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request.tools.iter().map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters})).collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    Value::Object(body)
}

fn responses_finish(response: &Value, has_calls: bool) -> String {
    match response["status"].as_str() {
        Some("completed") if has_calls => "tool_calls".into(),
        Some("completed") => "stop".into(),
        Some("incomplete") => match response["incomplete_details"]["reason"].as_str() {
            Some("max_output_tokens") => "length".into(),
            Some("content_filter") => "content_filter".into(),
            _ => "other".into(),
        },
        _ => "other".into(),
    }
}

fn responses_parse(value: &Value) -> Result<GenResponse> {
    if value["status"].as_str() == Some("failed") {
        return Err(Error::Remote("server_error".into()));
    }
    let mut out = GenResponse::default();
    let mut refused = false;
    for item in value["output"].as_array().ok_or(Error::Remote("malformed_response".into()))? {
        match item["type"].as_str() {
            Some("message") => {
                for content in item["content"].as_array().into_iter().flatten() {
                    match content["type"].as_str() {
                        Some("output_text") => out.text.push_str(content["text"].as_str().unwrap_or("")),
                        Some("refusal") => refused = true,
                        _ => {}
                    }
                }
            }
            Some("function_call") => out.tool_calls.push(ToolCall { name: item["name"].as_str().unwrap_or("").to_owned(), arguments: item["arguments"].as_str().unwrap_or("").to_owned() }),
            _ => {}
        }
    }
    out.finish = if refused { "content_filter".into() } else { responses_finish(value, !out.tool_calls.is_empty()) };
    out.usage = Usage { input_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0), output_tokens: value["usage"]["output_tokens"].as_u64().unwrap_or(0) };
    out.model = value["model"].as_str().map(str::to_owned);
    Ok(out)
}

fn responses_stream(reader: Box<dyn std::io::Read + Send>, cancel: &AtomicBool, on_delta: &mut dyn FnMut(&str)) -> Result<GenResponse> {
    let mut out = GenResponse { finish: "other".into(), ..Default::default() };
    let mut finished = false;
    transport::read_sse(reader, cancel, |event| {
        let payload: Value = serde_json::from_str(&event.data).map_err(|_| Error::Remote("malformed_stream".into()))?;
        let kind = payload["type"].as_str().or(event.event.as_deref()).unwrap_or("").to_owned();
        match kind.as_str() {
            "response.output_text.delta" => {
                if let Some(delta) = payload["delta"].as_str() {
                    out.text.push_str(delta);
                    out.stream_deltas += 1;
                    on_delta(delta);
                }
            }
            "response.output_item.done" => {
                let item = &payload["item"];
                if item["type"].as_str() == Some("function_call") {
                    out.tool_calls.push(ToolCall { name: item["name"].as_str().unwrap_or("").to_owned(), arguments: item["arguments"].as_str().unwrap_or("").to_owned() });
                }
                if item["type"].as_str() == Some("message") && item["content"].as_array().is_some_and(|c| c.iter().any(|p| p["type"] == "refusal")) {
                    out.finish = "content_filter".into();
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = &payload["response"];
                if out.finish != "content_filter" {
                    out.finish = responses_finish(response, !out.tool_calls.is_empty());
                }
                out.usage = Usage { input_tokens: response["usage"]["input_tokens"].as_u64().unwrap_or(0), output_tokens: response["usage"]["output_tokens"].as_u64().unwrap_or(0) };
                out.model = response["model"].as_str().map(str::to_owned);
                finished = true;
                return Ok(false);
            }
            "response.failed" | "error" => return Err(Error::Remote(refine_error(500, Some(&payload["response"]).filter(|v| !v.is_null()).or(Some(&payload))).into())),
            _ => {}
        }
        Ok(true)
    })?;
    if !finished {
        return Err(Error::Remote("stream_incomplete".into()));
    }
    Ok(out)
}

// ----------------------------------------------------------------- Anthropic

fn anthropic_body(profile: &Profile, request: &GenRequest) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            let content: Vec<Value> = message
                .parts
                .iter()
                .filter_map(|part| match part {
                    Part::Text(text) => Some(json!({"type": "text", "text": text})),
                    Part::ImagePng(url) => {
                        let data = url.strip_prefix("data:image/png;base64,")?;
                        Some(json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}}))
                    }
                    // The Messages API has no audio input; probes report it unsupported.
                    Part::AudioWav(_) => None,
                })
                .collect();
            json!({"role": message.role, "content": content})
        })
        .collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(profile.model_id));
    body.insert("system".into(), json!(request.system));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_tokens".into(), json!(request.max_output_tokens));
    body.insert("stream".into(), json!(request.stream));
    let mut tools: Vec<Value> = request.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters})).collect();
    if let OutputMode::JsonSchema { name, schema } = &request.output {
        // Structured output through a forced tool whose input is the result.
        tools.push(json!({"name": name, "description": "Return the result in this exact structure.", "input_schema": schema}));
        body.insert("tool_choice".into(), json!({"type": "tool", "name": name}));
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    Value::Object(body)
}

fn anthropic_finish(reason: Option<&str>) -> String {
    match reason {
        Some("end_turn") | Some("stop_sequence") => "stop",
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "other",
    }
    .into()
}

fn anthropic_parse(value: &Value) -> Result<GenResponse> {
    let mut out = GenResponse::default();
    for block in value["content"].as_array().ok_or(Error::Remote("malformed_response".into()))? {
        match block["type"].as_str() {
            Some("text") => out.text.push_str(block["text"].as_str().unwrap_or("")),
            Some("tool_use") => out.tool_calls.push(ToolCall { name: block["name"].as_str().unwrap_or("").to_owned(), arguments: block["input"].to_string() }),
            _ => {}
        }
    }
    out.finish = anthropic_finish(value["stop_reason"].as_str());
    out.usage = Usage { input_tokens: value["usage"]["input_tokens"].as_u64().unwrap_or(0), output_tokens: value["usage"]["output_tokens"].as_u64().unwrap_or(0) };
    out.model = value["model"].as_str().map(str::to_owned);
    Ok(out)
}

fn anthropic_stream(reader: Box<dyn std::io::Read + Send>, cancel: &AtomicBool, on_delta: &mut dyn FnMut(&str)) -> Result<GenResponse> {
    let mut out = GenResponse { finish: "other".into(), ..Default::default() };
    // Content blocks by index: (tool name if tool_use, accumulated input JSON).
    let mut blocks: Vec<Option<(String, String)>> = vec![];
    let mut stopped = false;
    transport::read_sse(reader, cancel, |event| {
        let payload: Value = serde_json::from_str(&event.data).map_err(|_| Error::Remote("malformed_stream".into()))?;
        let kind = payload["type"].as_str().or(event.event.as_deref()).unwrap_or("").to_owned();
        match kind.as_str() {
            "message_start" => {
                out.usage.input_tokens = payload["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
                out.model = payload["message"]["model"].as_str().map(str::to_owned);
            }
            "content_block_start" => {
                let index = stream_index(&payload["index"])?;
                while blocks.len() <= index {
                    blocks.push(None);
                }
                if payload["content_block"]["type"].as_str() == Some("tool_use") {
                    blocks[index] = Some((payload["content_block"]["name"].as_str().unwrap_or("").to_owned(), String::new()));
                }
            }
            "content_block_delta" => {
                let index = stream_index(&payload["index"])?;
                match payload["delta"]["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) = payload["delta"]["text"].as_str() {
                            out.text.push_str(text);
                            out.stream_deltas += 1;
                            on_delta(text);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(Some((_, input))) = blocks.get_mut(index) {
                            input.push_str(payload["delta"]["partial_json"].as_str().unwrap_or(""));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = payload["delta"]["stop_reason"].as_str() {
                    out.finish = anthropic_finish(Some(reason));
                }
                if let Some(tokens) = payload["usage"]["output_tokens"].as_u64() {
                    out.usage.output_tokens = tokens;
                }
            }
            "message_stop" => {
                stopped = true;
                return Ok(false);
            }
            "error" => return Err(Error::Remote(refine_error(500, Some(&payload)).into())),
            _ => {}
        }
        Ok(true)
    })?;
    if !stopped {
        return Err(Error::Remote("stream_incomplete".into()));
    }
    out.tool_calls = blocks.into_iter().flatten().map(|(name, input)| ToolCall { name, arguments: if input.is_empty() { "{}".into() } else { input } }).collect();
    Ok(out)
}

