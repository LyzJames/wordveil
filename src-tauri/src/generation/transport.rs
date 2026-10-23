//! HTTPS transport for user-configured generation endpoints.
//!
//! One request per call, no redirects (a redirect could move the request to an
//! unapproved host), HTTPS required except for loopback endpoints, Windows
//! SChannel with the system certificate store, and an optional proxy taken from
//! the Windows user proxy settings. Errors are mapped to short stable codes; a
//! response body is never logged or copied into an error.
use crate::{Error, Result};
use std::{
    io::{BufRead, BufReader, Read},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// Endpoint as configured by the user: scheme, host and base path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub base_path: String,
}

impl Endpoint {
    /// Parse and normalise a base URL. Credentials, queries and fragments are
    /// rejected; a full operation path such as `/chat/completions` is rejected
    /// so the adapter always appends the protocol path itself.
    pub fn parse(base_url: &str) -> Result<Self> {
        let url = base_url.trim();
        let (scheme, rest) = url.split_once("://").ok_or(Error::NeedsInput("base_url"))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "https" && scheme != "http" {
            return Err(Error::NeedsInput("base_url"));
        }
        if rest.contains('?') || rest.contains('#') || rest.contains('@') || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::NeedsInput("base_url"));
        }
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            let (inner, after) = stripped.split_once(']').ok_or(Error::NeedsInput("base_url"))?;
            if (!after.is_empty()&&!after.starts_with(':')) || inner.parse::<std::net::Ipv6Addr>().is_err(){return Err(Error::NeedsInput("base_url"));}
            let port = after.strip_prefix(':').map(|p| p.parse::<u16>().map_err(|_| Error::NeedsInput("base_url"))).transpose()?;
            (format!("[{inner}]"), port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host.to_owned(), Some(port.parse::<u16>().map_err(|_| Error::NeedsInput("base_url"))?)),
                None => (authority.to_owned(), None),
            }
        };
        let host = host.to_ascii_lowercase();
        if host.is_empty() || host.len() > 253 {
            return Err(Error::NeedsInput("base_url"));
        }
        let base_path = path.trim_end_matches('/').to_owned();
        let lowered = base_path.to_ascii_lowercase();
        if ["/chat/completions", "/responses", "/messages", "/completions"].iter().any(|suffix| lowered.ends_with(suffix)) {
            return Err(Error::NeedsInput("base_url_operation_path"));
        }
        let endpoint = Endpoint { scheme, host, port, base_path };
        if endpoint.scheme == "http" && !endpoint.is_loopback() {
            return Err(Error::NeedsInput("base_url_https"));
        }
        Ok(endpoint)
    }

    pub fn is_loopback(&self) -> bool {
        self.host == "localhost"
            || self.host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok_and(|address| address.is_loopback())
    }

    pub fn origin(&self) -> String {
        match self.port {
            Some(port) => format!("{}://{}:{port}", self.scheme, self.host),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }

    pub fn base_url(&self) -> String {
        format!("{}{}", self.origin(), self.base_path)
    }

    pub fn url(&self, operation: &str) -> String {
        format!("{}{}{}", self.origin(), self.base_path, operation)
    }

    /// Host shown in authorisation dialogs and recorded with each request.
    pub fn display_host(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyMode {
    /// Windows user proxy settings (WinINET), used by browsers and most apps.
    System,
    None,
}

#[derive(Clone, Debug)]
pub struct HttpOptions {
    pub timeout: Duration,
    pub proxy: ProxyMode,
}

/// Map an HTTP status to a stable code shared by every adapter.
pub fn status_code(status: u16) -> &'static str {
    match status {
        400 | 422 => "bad_request",
        401 => "auth_failed",
        402 => "insufficient_balance",
        403 => "forbidden",
        404 => "not_found",
        408 => "timeout",
        413 => "too_large",
        429 => "rate_limited",
        500..=599 => "server_error",
        _ => "http_error",
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub retry_after: Option<u64>,
    reader: Box<dyn Read + Send>,
}

impl HttpResponse {
    pub fn read_all(mut self, limit: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        (&mut self.reader).take(limit as u64 + 1).read_to_end(&mut out).map_err(|e| io_error(&e))?;
        if out.len() > limit {
            return Err(Error::Remote("response_too_large".into()));
        }
        Ok(out)
    }

    pub fn into_reader(self) -> Box<dyn Read + Send> {
        self.reader
    }
}

fn io_error(error: &std::io::Error) -> Error {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Error::Remote("timeout".into()),
        _ => Error::Remote("network".into()),
    }
}

fn ureq_error(error: ureq::Error) -> Error {
    match error {
        ureq::Error::Timeout(_) => Error::Remote("timeout".into()),
        ureq::Error::StatusCode(code) => Error::Remote(status_code(code).into()),
        ureq::Error::HostNotFound => Error::Remote("host_not_found".into()),
        ureq::Error::ConnectionFailed => Error::Remote("connection_failed".into()),
        ureq::Error::Tls(_) => Error::Remote("tls".into()),
        ureq::Error::RedirectFailed | ureq::Error::TooManyRedirects => Error::Remote("redirect_refused".into()),
        ureq::Error::Io(error) => io_error(&error),
        _ => Error::Remote("network".into()),
    }
}

/// Windows user proxy (WinINET): `ProxyEnable` and `ProxyServer` under the
/// current user's Internet Settings. Only reads the registry.
#[cfg(windows)]
pub fn system_proxy() -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings\0".encode_utf16().collect();
    let enable_name: Vec<u16> = "ProxyEnable\0".encode_utf16().collect();
    let server_name: Vec<u16> = "ProxyServer\0".encode_utf16().collect();
    unsafe {
        let mut enabled: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), enable_name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut enabled as *mut u32).cast(), &mut size);
        if status != 0 || enabled == 0 {
            return None;
        }
        let mut buffer = vec![0u16; 1024];
        let mut bytes = (buffer.len() * 2) as u32;
        let status = RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), server_name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut bytes);
        if status != 0 {
            return None;
        }
        let length = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        parse_proxy_server(&String::from_utf16_lossy(&buffer[..length]))
    }
}

#[cfg(not(windows))]
pub fn system_proxy() -> Option<String> {
    None
}

/// `ProxyServer` is either `host:port` or `http=h:p;https=h:p;socks=h:p`.
pub fn parse_proxy_server(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let chosen = if value.contains('=') {
        let mut https = None;
        let mut http = None;
        for part in value.split(';') {
            if let Some((scheme, address)) = part.split_once('=') {
                match scheme.trim().to_ascii_lowercase().as_str() {
                    "https" => https = Some(address.trim().to_owned()),
                    "http" => http = Some(address.trim().to_owned()),
                    _ => {}
                }
            }
        }
        https.or(http)?
    } else {
        value.split(';').next()?.trim().to_owned()
    };
    if chosen.is_empty() || chosen.contains(char::is_whitespace) {
        return None;
    }
    Some(if chosen.contains("://") { chosen } else { format!("http://{chosen}") })
}

fn agent(endpoint: &Endpoint, options: &HttpOptions) -> Result<ureq::Agent> {
    let tls = ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::NativeTls).build();
    let proxy = match (&options.proxy, endpoint.is_loopback()) {
        (ProxyMode::System, false) => system_proxy().and_then(|address| ureq::Proxy::new(&address).ok()),
        _ => None,
    };
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(15).min(options.timeout)))
        .timeout_recv_response(Some(options.timeout))
        .timeout_global(Some(options.timeout + Duration::from_secs(30)))
        .proxy(proxy)
        .tls_config(tls)
        .user_agent(concat!("lvjin/", env!("CARGO_PKG_VERSION")))
        .build();
    Ok(ureq::Agent::new_with_config(config))
}

/// POST a JSON body. `headers` never appear in logs or errors.
pub fn post_json(endpoint: &Endpoint, operation: &str, headers: &[(&str, &str)], body: &[u8], options: &HttpOptions, accept_stream: bool) -> Result<HttpResponse> {
    #[cfg(feature="m0-harness")]
    crate::network::charge_live(&endpoint.display_host(),operation)?;
    let agent = agent(endpoint, options)?;
    let mut request = agent.post(endpoint.url(operation)).header("Content-Type", "application/json");
    request = request.header("Accept", if accept_stream { "text/event-stream" } else { "application/json" });
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send(body).map_err(ureq_error)?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(Error::Remote("redirect_refused".into()));
    }
    let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
    Ok(HttpResponse { status, retry_after, reader })
}

/// GET (used for optional model listing by the capability self-test).
pub fn get(endpoint: &Endpoint, operation: &str, headers: &[(&str, &str)], options: &HttpOptions) -> Result<HttpResponse> {
    #[cfg(feature="m0-harness")]
    crate::network::charge_live(&endpoint.display_host(),operation)?;
    let agent = agent(endpoint, options)?;
    let mut request = agent.get(endpoint.url(operation)).header("Accept", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.call().map_err(ureq_error)?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(Error::Remote("redirect_refused".into()));
    }
    let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
    Ok(HttpResponse { status, retry_after: None, reader })
}

/// One server-sent event: the `event:` name (if any) and joined `data:` lines.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Iterate SSE events from a byte stream, honouring cancellation between lines.
/// A single event is bounded (1 MiB) so a hostile stream cannot exhaust memory.
pub fn read_sse(reader: Box<dyn Read + Send>, cancel: &AtomicBool, mut on_event: impl FnMut(SseEvent) -> Result<bool>) -> Result<()> {
    const EVENT_LIMIT: usize = 1 << 20;
    let mut lines = BufReader::new(reader);
    let mut event: Option<String> = None;
    let mut data = String::new();
    let mut line = String::new();
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        line.clear();
        // A single line is bounded as well: a stream that never sends a
        // newline cannot grow memory without limit.
        let read = (&mut lines).take(EVENT_LIMIT as u64 + 2).read_line(&mut line).map_err(|e| io_error(&e))?;
        if read > EVENT_LIMIT && !line.ends_with('\n') {
            return Err(Error::Remote("stream_event_too_large".into()));
        }
        if read == 0 {
            if !data.is_empty() {
                on_event(SseEvent { event: event.take(), data: std::mem::take(&mut data) })?;
            }
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !data.is_empty() || event.is_some() {
                let keep_going = on_event(SseEvent { event: event.take(), data: std::mem::take(&mut data) })?;
                if !keep_going {
                    return Ok(());
                }
            }
            continue;
        }
        if trimmed.starts_with(':') {
            continue;
        }
        let (field, value) = match trimmed.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (trimmed, ""),
        };
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
                if data.len() > EVENT_LIMIT {
                    return Err(Error::Remote("stream_event_too_large".into()));
                }
            }
            _ => {}
        }
    }
}
