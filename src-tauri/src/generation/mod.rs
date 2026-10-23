//! User-managed generation connections (design 8.2-8.4, contract section 2).
//!
//! The application never deploys a generation model. It stores the user's
//! connection profile in the encrypted vault, the API key under user-scoped
//! DPAPI, and talks to the endpoint through one of three protocol adapters.
//! Every real request is dispatched through the root ledger first, so budgets,
//! cancellation and the authorised destination are checked before anything
//! leaves the machine.
pub mod adapters;
pub mod connections;
pub mod credentials;
pub mod probe;
pub mod transport;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{RecvTimeoutError, channel},
    },
    time::Duration,
};
use zeroize::Zeroizing;

/// Bumped whenever request building or response parsing changes; capability
/// results recorded with an older adapter version must be re-tested.
pub const ADAPTER_VERSION: &str = "1.0";

/// One request the caller can stop waiting for at once (cancel, lock, exit).
/// The blocking HTTP exchange runs on its own thread; when the caller gives
/// up, that thread ends on its own (reply or connection timeout) and its
/// result is discarded. The outcome is then unknown and never retried here.
pub fn call_cancellable(profile: &Profile, key: Option<&Zeroizing<String>>, request: &GenRequest, cancel: &AtomicBool) -> crate::Result<GenResponse> {
    call_with_permit(profile, key, request, cancel, None)
}

pub fn call_cancellable_with_slot(profile: &Profile, key: Option<&Zeroizing<String>>, request: &GenRequest, cancel: &AtomicBool, slot: crate::services::SlotGuard) -> crate::Result<GenResponse> {
    call_with_permit(profile, key, request, cancel, Some(slot))
}

fn call_with_permit(profile: &Profile, key: Option<&Zeroizing<String>>, request: &GenRequest, cancel: &AtomicBool, slot: Option<crate::services::SlotGuard>) -> crate::Result<GenResponse> {
    if cancel.load(Ordering::Acquire) { return Err(crate::Error::Cancelled); }
    let (sender, receiver) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let (profile, key, request, inner) = (profile.clone(), key.cloned(), request.clone(), stop.clone());
    std::thread::Builder::new().name("lvjin-generation".into()).spawn(move || {
        let _slot = slot;
        let mut ignore = |_: &str| {};
        let _ = sender.send(adapters::call(&profile, key.as_ref(), &request, &inner, &mut ignore));
    })?;
    loop {
        if cancel.load(Ordering::Acquire) {
            stop.store(true, Ordering::Release);
            return Err(crate::Error::Cancelled);
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(outcome) => return outcome,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(crate::Error::Integrity),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// OpenAI-compatible Chat Completions (OpenAI, DeepSeek, local servers).
    ChatCompletions,
    /// OpenAI Responses.
    Responses,
    /// Anthropic Messages.
    AnthropicMessages,
}

impl Protocol {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "chat_completions" => Some(Self::ChatCompletions),
            "responses" => Some(Self::Responses),
            "anthropic_messages" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Deepseek,
    Openai,
    Anthropic,
    Local,
    Custom,
}

impl Provider {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "deepseek" => Some(Self::Deepseek),
            "openai" => Some(Self::Openai),
            "anthropic" => Some(Self::Anthropic),
            "local" => Some(Self::Local),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deepseek => "deepseek",
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Local => "local",
            Self::Custom => "custom",
        }
    }
}

/// How the request asks for structured output.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputMode {
    Text,
    /// JSON object mode: syntax only; the core validates the schema afterwards.
    JsonObject,
    /// Strict JSON Schema (OpenAI `json_schema`, Anthropic forced tool input).
    JsonSchema { name: String, schema: Value },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    /// `data:image/png;base64,...` built by the core from synthetic or approved media.
    ImagePng(String),
    /// Base64 WAV (16 kHz mono) for audio-input probes.
    AudioWav(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: &'static str,
    pub parts: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug)]
pub struct GenRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub max_output_tokens: u32,
    pub output: OutputMode,
    pub tools: Vec<ToolSpec>,
    pub stream: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GenResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    /// Normalised: stop, length, tool_calls, content_filter, other.
    pub finish: String,
    pub usage: Usage,
    pub model: Option<String>,
    /// Number of streamed text deltas (0 for non-streaming responses).
    pub stream_deltas: u32,
}

/// Connection profile as used by the adapters (no key).
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub provider: Provider,
    pub protocol: Protocol,
    pub endpoint: transport::Endpoint,
    pub model_id: String,
    pub timeout_s: u64,
    pub max_output_tokens: u32,
    pub proxy: transport::ProxyMode,
}

impl Profile {
    pub fn http(&self) -> transport::HttpOptions {
        transport::HttpOptions { timeout: std::time::Duration::from_secs(self.timeout_s), proxy: self.proxy.clone() }
    }

    /// Identity of the tested combination; any change invalidates capabilities.
    pub fn binding(&self) -> String {
        crate::store::sha256_hex(format!("{}|{}|{}|{}", self.protocol.as_str(), self.endpoint.base_url(), self.model_id, ADAPTER_VERSION).as_bytes())
    }
}
