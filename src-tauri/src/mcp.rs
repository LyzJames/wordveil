//! The official MCP SDK handles version negotiation, handshake and mirrored
//! headers. Our HTTP backend adds a fixed destination, byte/request limits,
//! no redirects/retries and stable errors without remote text or credentials.
use crate::{Error, Result, generation::transport::{Endpoint, HttpOptions, ProxyMode}, services::Slots, store::sha256_hex};
use rmcp::{ClientServiceExt, ClientLifecycleMode, model::{ClientCapabilities, ClientConfig, Implementation, CallToolRequestParams, ClientJsonRpcMessage, ServerJsonRpcMessage, ProtocolVersion},
    transport::{StreamableHttpClientTransport, streamable_http_client::{StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse, SseError}}};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU32, Ordering}, mpsc}, time::Duration};
use zeroize::Zeroizing;

pub const SDK_VERSION: &str = "rmcp/3.6.0";
pub const MAX_EXCHANGES: u32 = 12;
type WireError = StreamableHttpError<std::io::Error>;
pub type RequestGuard = Arc<dyn Fn()->Result<()> + Send + Sync>;

#[derive(Clone)]
struct BoundedHttp {
    client: reqwest::Client,
    endpoint: Arc<str>,
    key: Arc<Option<Zeroizing<String>>>,
    calls: Arc<AtomicU32>,
    last_error: Arc<Mutex<Option<String>>>,
    guard: Option<RequestGuard>,
    stopped: Arc<AtomicBool>,
}
impl BoundedHttp {
    fn failed(&self, code: &str) -> WireError {
        if let Ok(mut last) = self.last_error.lock() { *last = Some(code.into()); }
        StreamableHttpError::Io(std::io::Error::other(code))
    }
    async fn post(&self, uri: Arc<str>, message: ClientJsonRpcMessage, session: Option<Arc<str>>, headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>) -> std::result::Result<StreamableHttpPostResponse, WireError> {
        if uri != self.endpoint { return Err(self.failed("destination_changed")); }
        if self.stopped.load(Ordering::Acquire){return Err(self.failed("cancelled"));}
        if self.guard.as_ref().is_some_and(|guard|guard().is_err()){return Err(self.failed("authorization_revoked"));}
        if self.calls.fetch_add(1, Ordering::AcqRel) >= MAX_EXCHANGES { return Err(self.failed("mcp_request_budget")); }
        let wire = serde_json::to_value(&message).map_err(|_| self.failed("mcp_contract"))?;
        #[cfg(feature="test-harness")]
        crate::network::charge_live(uri.as_ref(),wire["method"].as_str().unwrap_or("protocol_reply")).map_err(|_|self.failed("live_request_budget"))?;
        // Unknown server requests never gain a client capability. Only bounded
        // protocol replies/cancellation can accompany this one read-only call.
        let mut request = self.client.post(uri.as_ref()).headers(headers.into_iter().collect()).header("Accept", "application/json, text/event-stream").json(&wire);
        if let Some(key) = self.key.as_ref() { request = request.bearer_auth(key.as_str()); }
        if let Some(session) = session { request = request.header("Mcp-Session-Id", session.as_ref()); }
        let mut response = request.send().await.map_err(|e| self.failed(if e.is_timeout() { "timeout" } else { "network" }))?;
        let status = response.status().as_u16();
        if status == 202 { return Ok(StreamableHttpPostResponse::Accepted); }
        let session = response.headers().get("Mcp-Session-Id").and_then(|s| s.to_str().ok()).filter(|s| s.len() <= 256 && s.bytes().all(|b| b.is_ascii_graphic())).map(str::to_owned);
        let content_type = response.headers().get("Content-Type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let legacy_rejection=wire["method"]=="server/discover" && matches!(status,400|404|405|422);
        if !(200..300).contains(&status) && status != 400 && !legacy_rejection { return Err(self.failed(if (300..400).contains(&status){"redirect_refused"}else{crate::generation::transport::status_code(status)})); }
        if !content_type.starts_with("application/json") && !content_type.starts_with("text/event-stream") && !legacy_rejection { return Err(self.failed("mcp_content_type")); }
        if response.content_length().is_some_and(|n| n > crate::network::MAX_BODY as u64) { return Err(self.failed("response_too_large")); }
        let mut bytes = vec![];
        while let Some(chunk) = response.chunk().await.map_err(|e| self.failed(if e.is_timeout() { "timeout" } else { "network" }))? {
            if bytes.len() + chunk.len() > crate::network::MAX_BODY { return Err(self.failed("response_too_large")); }
            bytes.extend_from_slice(&chunk);
            if content_type.starts_with("text/event-stream") {
                if let Some(value) = sse_response(&bytes).map_err(|_| self.failed("mcp_server_request_refused"))? {
                    return Ok(StreamableHttpPostResponse::Json(value, session));
                }
            }
        }
        if content_type.starts_with("text/event-stream") { return Err(self.failed("malformed_response")); }
        let value = match serde_json::from_slice::<ServerJsonRpcMessage>(&bytes){
            Ok(value)=>value,
            Err(_) if legacy_rejection => {
                serde_json::from_value(json!({"jsonrpc":"2.0","id":wire["id"],"error":{"code":-32601,"message":"legacy transport requires initialization"}})).map_err(|_|self.failed("malformed_response"))?
            }
            Err(_)=>return Err(self.failed("malformed_response")),
        };
        // A legacy server often rejects discover with an unstructured HTTP400.
        // Preserve a *synthetic* method-not-found error for SDK era fallback;
        // no retry of a business tools/call is allowed.
        Ok(StreamableHttpPostResponse::Json(value, session))
    }
}

fn sse_response(bytes: &[u8]) -> std::result::Result<Option<ServerJsonRpcMessage>, ()> {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let end = text.rfind("\n\n").map(|i| i + 2).unwrap_or(0);
    for event in text[..end].split("\n\n") {
        let data = event.lines().filter_map(|l| l.strip_prefix("data:").map(str::trim_start)).collect::<Vec<_>>().join("\n");
        if data.is_empty() { continue; }
        let value: Value = serde_json::from_str(&data).map_err(|_| ())?;
        if value.get("method").is_some() && value.get("id").is_some() { return Err(()); }
        if value.get("result").is_some() || value.get("error").is_some() {
            return serde_json::from_value(value).map(Some).map_err(|_| ());
        }
    }
    Ok(None)
}

impl StreamableHttpClient for BoundedHttp {
    type Error = std::io::Error;
    async fn post_message(&self, uri: Arc<str>, message: ClientJsonRpcMessage, session: Option<Arc<str>>, _auth: Option<String>, headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>) -> std::result::Result<StreamableHttpPostResponse, WireError> {
        self.post(uri, message, session, headers).await
    }
    async fn delete_session(&self, _uri: Arc<str>, _session: Arc<str>, _auth: Option<String>, _headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>) -> std::result::Result<(), WireError> {
        // Short-lived clients do not issue cleanup requests with an unknown
        // outcome. Legacy sessions expire server-side; modern MCP has none.
        Err(StreamableHttpError::ServerDoesNotSupportDeleteSession)
    }
    async fn get_stream(&self, _uri: Arc<str>, _session: Option<Arc<str>>, _last: Option<String>, _auth: Option<String>, _headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>) -> std::result::Result<futures::stream::BoxStream<'static, std::result::Result<sse_stream::Sse, SseError>>, WireError> {
        // This client advertises no server-input capabilities. It consumes
        // only responses on the POST that the user approved.
        Err(StreamableHttpError::ServerDoesNotSupportSse)
    }
}

pub fn validate_schema(schema: &Value) -> Result<()> {
    fn inspect(v: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
        *nodes += 1;
        if depth > 24 || *nodes > 4000 { return Err(Error::Budget); }
        match v {
            Value::Object(map) => {
                if map.get("$ref").and_then(Value::as_str).is_some() || map.contains_key("$dynamicRef") { return Err(Error::Denied); }
                for item in map.values() { inspect(item, depth + 1, nodes)?; }
            }
            Value::Array(a) => for item in a { inspect(item, depth + 1, nodes)?; },
            Value::String(s) if s.len() > 32000 => return Err(Error::Budget),
            _ => {},
        }
        Ok(())
    }
    inspect(schema, 0, &mut 0)?;
    if schema["type"] != "object" || !schema["properties"].is_object() || schema.to_string().len() > 128000 { return Err(Error::Contract); }
    jsonschema::validator_for(schema).map_err(|_| Error::Contract)?;
    Ok(())
}
pub fn validate_arguments(schema: &Value, args: &Value) -> Result<()> {
    validate_schema(schema)?;
    if args.to_string().len() > 32768 || !args.is_object() { return Err(Error::Contract); }
    let fields = schema["properties"].as_object().ok_or(Error::Contract)?;
    if args.as_object().unwrap().keys().any(|k| !fields.contains_key(k)) { return Err(Error::Denied); }
    if !jsonschema::validator_for(schema).map_err(|_| Error::Contract)?.is_valid(args) { return Err(Error::Contract); }
    Ok(())
}
pub fn descriptor_hash(tool: &Value) -> String { sha256_hex(tool.to_string().as_bytes()) }

pub struct Call {
    pub name: String,
    pub descriptor_hash: String,
    pub server_hash: String,
    pub arguments: Value,
}

pub fn exchange(endpoint: Endpoint, key: Option<Zeroizing<String>>, options: HttpOptions, call: Option<Call>, slots: &Slots, cancel: &AtomicBool, guard:Option<RequestGuard>) -> Result<Value> {
    let permit = slots.acquire(cancel)?;
    let timeout = options.timeout.min(Duration::from_secs(120));
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let (send, recv) = mpsc::channel();
    std::thread::spawn(move || {
        let _permit = permit;
        let result = (|| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_| Error::Integrity)?;
            rt.block_on(async move {
                let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(timeout).connect_timeout(Duration::from_secs(10)).no_proxy();
                if options.proxy == ProxyMode::System && !endpoint.is_loopback() {
                    if let Some(proxy) = crate::generation::transport::system_proxy() { builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| Error::Contract)?); }
                }
                let http = BoundedHttp { client: builder.build().map_err(|_| Error::Integrity)?, endpoint: endpoint.base_url().into(), key: Arc::new(key), calls: Arc::new(AtomicU32::new(0)), last_error: Arc::new(Mutex::new(None)), guard, stopped:stopped.clone() };
                let mut config = StreamableHttpClientTransportConfig::with_uri(http.endpoint.clone()).max_concurrent_requests(1).max_sse_event_size(crate::network::MAX_BODY).reinit_on_expired_session(false);
                config.allow_stateless = true;
                let transport = StreamableHttpClientTransport::with_client(http.clone(), config);
                let operation = async {
                    let config = ClientConfig::new(ClientCapabilities::default(), Implementation::new("lvjin-readonly-client", crate::VERSION));
                    let client = config.serve_with_lifecycle(transport,ClientLifecycleMode::Auto{preferred_versions:vec![ProtocolVersion::V_2026_07_28],legacy_version:Some(ProtocolVersion::V_2025_11_25)}).await.map_err(|_| Error::Remote("mcp_handshake".into()))?;
                    let info = serde_json::to_value(client.peer_info()).map_err(Error::from)?;
                    let server_identity = json!({"protocolVersion": info["protocolVersion"], "serverInfo": info["serverInfo"]});
                    let server_hash = sha256_hex(server_identity.to_string().as_bytes());
                    let all = client.list_all_tools().await.map_err(|_| Error::Remote("mcp_tools_list".into()))?;
                    if all.len() > 64 { return Err(Error::Budget); }
                    let tools = all.into_iter().map(serde_json::to_value).collect::<std::result::Result<Vec<_>, _>>()?;
                    let accepted = tools.into_iter().filter(|t| t["annotations"]["readOnlyHint"] == true && validate_schema(&t["inputSchema"]).is_ok()).collect::<Vec<_>>();
                    let result = if let Some(call) = call {
                        if call.server_hash != server_hash { return Err(Error::Remote("mcp_binding_changed".into())); }
                        let tool = accepted.iter().find(|t| t["name"] == call.name && descriptor_hash(t) == call.descriptor_hash).ok_or_else(|| Error::Remote("mcp_binding_changed".into()))?;
                        validate_arguments(&tool["inputSchema"], &call.arguments)?;
                        let reply = client.call_tool(CallToolRequestParams::new(call.name).with_arguments(call.arguments.as_object().cloned().ok_or(Error::Contract)?)).await.map_err(|_| Error::Remote("mcp_tool_call".into()))?;
                        let reply = serde_json::to_value(reply)?;
                        if reply.to_string().len() > crate::network::MAX_BODY || reply.get("inputRequests").is_some() || reply.get("requestState").is_some() { return Err(Error::Remote("mcp_interaction_refused".into())); }
                        if reply["isError"] == true { return Err(Error::Remote("mcp_tool_error".into())); }
                        if let Some(schema) = tool.get("outputSchema") {
                            validate_schema(schema)?;
                            if !jsonschema::validator_for(schema).map_err(|_| Error::Contract)?.is_valid(&reply["structuredContent"]) { return Err(Error::Remote("mcp_output_schema".into())); }
                        }
                        json!({"response": reply})
                    } else { json!({"tools": accepted}) };
                    let _ = client.cancel().await;
                    Ok(json!({"sdk": SDK_VERSION, "server": server_identity, "server_hash": server_hash, "result": result, "exchanges": http.calls.load(Ordering::Acquire)}))
                };
                let result = tokio::select! {
                    result = tokio::time::timeout(timeout, operation) => result.unwrap_or_else(|_| Err(Error::Remote("timeout".into()))),
                    _ = async { while !stopped.load(Ordering::Acquire) { tokio::time::sleep(Duration::from_millis(25)).await; } } => Err(Error::Cancelled),
                };
                result.map_err(|error| http.last_error.lock().ok().and_then(|v| v.clone()).map(Error::Remote).unwrap_or(error))
            })
        })();
        let _ = send.send(result);
    });
    loop {
        if cancel.load(Ordering::Acquire) { stop.store(true, Ordering::Release); return Err(Error::Cancelled); }
        match recv.recv_timeout(Duration::from_millis(25)) {
            Ok(value) => return value,
            Err(mpsc::RecvTimeoutError::Timeout) => {},
            Err(_) => return Err(Error::Integrity),
        }
    }
}
