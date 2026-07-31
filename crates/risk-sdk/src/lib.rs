use serde_json::Value;
use std::error::Error as StdError;
use std::ffi::{c_char, CStr};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use ureq::OrAnyStatus;
use url::Url;

pub const PGR_ABI_VERSION: u32 = 1;
pub const PGR_OK: i32 = 0;
pub const PGR_ERR_INVALID_ARGUMENT: i32 = -1;
pub const PGR_ERR_RETRY: i32 = -2;
pub const PGR_ERR_TIMEOUT: i32 = -3;
pub const PGR_ERR_BUFFER_TOO_SMALL: i32 = -4;
pub const PGR_ERR_AGENT_REJECTED: i32 = -5;
pub const PGR_ERR_STATE: i32 = -6;
pub const PGR_ERR_INTERNAL: i32 = -7;

const MAX_BATCH_BYTES: usize = 256 * 1024;
const MAX_DECISION_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const DEFAULT_QUEUE_CAPACITY: usize = 256;
const MAX_QUEUE_CAPACITY: usize = 16_384;

#[repr(C)]
pub struct PgrConfigV1 {
    pub abi_version: u32,
    pub endpoint_utf8: *const c_char,
    pub local_token_utf8: *const c_char,
    pub emit_timeout_ms: u32,
    pub check_timeout_ms: u32,
    pub queue_capacity: u32,
}

#[derive(Clone)]
struct HttpClient {
    transport: ClientTransport,
    token: Arc<str>,
    timeout: Duration,
}

#[derive(Clone)]
enum ClientTransport {
    Local(SocketAddrV4),
    Remote {
        base_url: Arc<str>,
        agent: ureq::Agent,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EndpointConfig {
    Local(SocketAddrV4),
    Remote(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientError {
    Timeout,
    Other,
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

struct PendingState {
    count: Mutex<usize>,
    changed: Condvar,
    rejected: AtomicUsize,
}

struct SdkRuntime {
    sender: SyncSender<Vec<u8>>,
    client: HttpClient,
    pending: Arc<PendingState>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    emit_timeout: Duration,
}

static RUNTIME: OnceLock<Mutex<Option<SdkRuntime>>> = OnceLock::new();

fn runtime() -> &'static Mutex<Option<SdkRuntime>> {
    RUNTIME.get_or_init(|| Mutex::new(None))
}

fn parse_endpoint(endpoint: &str) -> Result<EndpointConfig, i32> {
    let parsed = Url::parse(endpoint).map_err(|_| PGR_ERR_INVALID_ARGUMENT)?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(PGR_ERR_INVALID_ARGUMENT);
    }
    match parsed.scheme() {
        "http"
            if parsed.host_str() == Some("127.0.0.1")
                && parsed.path() == "/"
                && parsed.port().is_some_and(|port| port != 0) =>
        {
            let port = parsed.port().ok_or(PGR_ERR_INVALID_ARGUMENT)?;
            Ok(EndpointConfig::Local(SocketAddrV4::new(
                Ipv4Addr::LOCALHOST,
                port,
            )))
        }
        "https"
            if parsed.host_str().is_some() && parsed.path().trim_end_matches('/') == "/sdk/v1" =>
        {
            Ok(EndpointConfig::Remote(
                parsed.as_str().trim_end_matches('/').to_string(),
            ))
        }
        _ => Err(PGR_ERR_INVALID_ARGUMENT),
    }
}

fn valid_token(token: &str) -> bool {
    (32..=256).contains(&token.len())
        && token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b'/' | b'=')
        })
}

unsafe fn required_cstr<'a>(value: *const c_char) -> Result<&'a str, i32> {
    if value.is_null() {
        return Err(PGR_ERR_INVALID_ARGUMENT);
    }
    CStr::from_ptr(value)
        .to_str()
        .map_err(|_| PGR_ERR_INVALID_ARGUMENT)
}

unsafe fn required_bytes<'a>(
    value: *const c_char,
    len: usize,
    max: usize,
) -> Result<&'a [u8], i32> {
    if value.is_null() || len == 0 || len > max {
        return Err(PGR_ERR_INVALID_ARGUMENT);
    }
    let bytes = std::slice::from_raw_parts(value.cast::<u8>(), len);
    std::str::from_utf8(bytes).map_err(|_| PGR_ERR_INVALID_ARGUMENT)?;
    Ok(bytes)
}

fn validate_batch(bytes: &[u8]) -> Result<(), i32> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| PGR_ERR_INVALID_ARGUMENT)?;
    let object = value.as_object().ok_or(PGR_ERR_INVALID_ARGUMENT)?;
    if object.get("schema_version").and_then(Value::as_str) != Some("1.0")
        || object.get("producer").and_then(Value::as_object).is_none()
        || object
            .get("events")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        return Err(PGR_ERR_INVALID_ARGUMENT);
    }
    Ok(())
}

fn validate_decision(bytes: &[u8]) -> Result<(), i32> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| PGR_ERR_INVALID_ARGUMENT)?;
    if value.as_object().is_none() {
        return Err(PGR_ERR_INVALID_ARGUMENT);
    }
    Ok(())
}

fn validate_json_object(bytes: &[u8]) -> Result<(), i32> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| PGR_ERR_INVALID_ARGUMENT)?;
    value
        .as_object()
        .map(|_| ())
        .ok_or(PGR_ERR_INVALID_ARGUMENT)
}

impl HttpClient {
    fn post(&self, path: &str, body: &[u8]) -> Result<HttpResponse, ClientError> {
        match &self.transport {
            ClientTransport::Local(address) => self.post_local(*address, path, body),
            ClientTransport::Remote { base_url, agent } => {
                let target = remote_target(base_url, path)?;
                let authorization = format!("Bearer {}", self.token);
                let response = agent
                    .post(&target)
                    .set("Content-Type", "application/json")
                    .set("Authorization", &authorization)
                    .send_bytes(body)
                    .or_any_status()
                    .map_err(map_remote_error)?;
                read_remote_response(response)
            }
        }
    }

    fn post_local(
        &self,
        address: SocketAddrV4,
        path: &str,
        body: &[u8],
    ) -> Result<HttpResponse, ClientError> {
        let mut stream =
            TcpStream::connect_timeout(&address.into(), self.timeout).map_err(map_io_error)?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(map_io_error)?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(map_io_error)?;
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nX-PGR-Local-Token: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.token,
            body.len()
        )
        .map_err(map_io_error)?;
        stream.write_all(body).map_err(map_io_error)?;
        stream.flush().map_err(map_io_error)?;

        let mut response = Vec::new();
        stream
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut response)
            .map_err(map_io_error)?;
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(ClientError::Other);
        }
        parse_http_response(response).map_err(map_io_error)
    }
}

fn remote_target(base_url: &str, agent_path: &str) -> Result<String, ClientError> {
    let suffix = agent_path
        .strip_prefix("/agent/v1")
        .ok_or(ClientError::Other)?;
    Ok(format!("{base_url}{suffix}"))
}

fn map_io_error(error: std::io::Error) -> ClientError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        ClientError::Timeout
    } else {
        ClientError::Other
    }
}

fn map_remote_error(error: ureq::Transport) -> ClientError {
    let mut source = error.source();
    while let Some(current) = source {
        if let Some(io_error) = current.downcast_ref::<std::io::Error>() {
            return map_io_error(std::io::Error::new(
                io_error.kind(),
                "remote request failed",
            ));
        }
        source = current.source();
    }
    ClientError::Other
}

fn read_remote_response(response: ureq::Response) -> Result<HttpResponse, ClientError> {
    let status = response.status();
    let mut body = Vec::new();
    response
        .into_reader()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(map_io_error)?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(ClientError::Other);
    }
    Ok(HttpResponse { status, body })
}

fn parse_http_response(response: Vec<u8>) -> std::io::Result<HttpResponse> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing headers"))?;
    let head = std::str::from_utf8(&response[..split])
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid headers"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid status"))?;
    Ok(HttpResponse {
        status,
        body: response[(split + 4)..].to_vec(),
    })
}

fn batch_ack_rejected(body: &[u8]) -> Option<bool> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    object.get("accepted")?.as_u64()?;
    object.get("duplicates")?.as_u64()?;
    Some(!object.get("rejected")?.as_array()?.is_empty())
}

fn valid_decision_response(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    matches!(
        object.get("mode").and_then(Value::as_str),
        Some("shadow" | "enforce")
    ) && matches!(
        object.get("decision").and_then(Value::as_str),
        Some("allow" | "review" | "deny")
    ) && object.get("decision_id").and_then(Value::as_str).is_some()
}

fn valid_action_response(path: &str, body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    match path {
        "/agent/v1/actions:pull" => object.get("actions").and_then(Value::as_array).is_some(),
        "/agent/v1/actions:ack" => object.get("ok").and_then(Value::as_bool) == Some(true),
        _ => false,
    }
}

fn pending_increment(pending: &PendingState) {
    *pending
        .count
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) += 1;
}

fn pending_complete(pending: &PendingState, rejected: bool) {
    if rejected {
        pending.rejected.fetch_add(1, Ordering::Relaxed);
    }
    let mut count = pending
        .count
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *count = count.saturating_sub(1);
    pending.changed.notify_all();
}

fn worker_loop(
    receiver: Receiver<Vec<u8>>,
    client: HttpClient,
    pending: Arc<PendingState>,
    stopping: Arc<AtomicBool>,
) {
    while let Ok(batch) = receiver.recv() {
        let mut backoff = Duration::from_millis(20);
        loop {
            if stopping.load(Ordering::Relaxed) {
                pending_complete(&pending, true);
                break;
            }
            match client.post("/agent/v1/events:batch", &batch) {
                Ok(response) if (200..300).contains(&response.status) => {
                    match batch_ack_rejected(&response.body) {
                        Some(rejected) => {
                            pending_complete(&pending, rejected);
                            break;
                        }
                        None => {
                            thread::sleep(backoff);
                            backoff = (backoff * 2).min(Duration::from_secs(1));
                        }
                    }
                }
                Ok(response) if (400..500).contains(&response.status) => {
                    pending_complete(&pending, true);
                    break;
                }
                _ => {
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(1));
                }
            }
        }
    }
}

fn send_with_timeout(runtime: &SdkRuntime, mut batch: Vec<u8>) -> i32 {
    pending_increment(&runtime.pending);
    let deadline = Instant::now() + runtime.emit_timeout;
    loop {
        match runtime.sender.try_send(batch) {
            Ok(()) => return PGR_OK,
            Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                batch = returned;
                thread::yield_now();
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                pending_complete(&runtime.pending, false);
                return PGR_ERR_RETRY;
            }
        }
    }
}

fn init_impl(config: *const PgrConfigV1) -> i32 {
    if config.is_null() {
        return PGR_ERR_INVALID_ARGUMENT;
    }
    let config = unsafe { &*config };
    if config.abi_version != PGR_ABI_VERSION {
        return PGR_ERR_INVALID_ARGUMENT;
    }
    let endpoint = match unsafe { required_cstr(config.endpoint_utf8) }.and_then(parse_endpoint) {
        Ok(value) => value,
        Err(code) => return code,
    };
    let token = match unsafe { required_cstr(config.local_token_utf8) } {
        Ok(value) if valid_token(value) => value,
        _ => return PGR_ERR_INVALID_ARGUMENT,
    };
    let capacity = if config.queue_capacity == 0 {
        DEFAULT_QUEUE_CAPACITY
    } else {
        config.queue_capacity as usize
    };
    if capacity > MAX_QUEUE_CAPACITY
        || config.emit_timeout_ms > 100
        || config.check_timeout_ms == 0
        || config.check_timeout_ms > 5_000
    {
        return PGR_ERR_INVALID_ARGUMENT;
    }

    let mut guard = runtime()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if guard.is_some() {
        return PGR_ERR_STATE;
    }

    let timeout = Duration::from_millis(config.check_timeout_ms.into());
    let transport = match endpoint {
        EndpointConfig::Local(address) => ClientTransport::Local(address),
        EndpointConfig::Remote(base_url) => ClientTransport::Remote {
            base_url: Arc::from(base_url),
            agent: ureq::AgentBuilder::new()
                .timeout(timeout)
                .timeout_connect(timeout)
                .redirects(0)
                .try_proxy_from_env(false)
                .build(),
        },
    };
    let client = HttpClient {
        transport,
        token: Arc::from(token),
        timeout,
    };
    let pending = Arc::new(PendingState {
        count: Mutex::new(0),
        changed: Condvar::new(),
        rejected: AtomicUsize::new(0),
    });
    let stopping = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(capacity);
    let worker_client = client.clone();
    let worker_pending = Arc::clone(&pending);
    let worker_stopping = Arc::clone(&stopping);
    let worker = match thread::Builder::new()
        .name("pgr-agent-writer".to_owned())
        .spawn(move || worker_loop(receiver, worker_client, worker_pending, worker_stopping))
    {
        Ok(worker) => worker,
        Err(_) => return PGR_ERR_INTERNAL,
    };
    *guard = Some(SdkRuntime {
        sender,
        client,
        pending,
        stopping,
        worker: Some(worker),
        emit_timeout: Duration::from_millis(config.emit_timeout_ms.into()),
    });
    PGR_OK
}

fn emit_impl(json_utf8: *const c_char, json_len: usize) -> i32 {
    let bytes = match unsafe { required_bytes(json_utf8, json_len, MAX_BATCH_BYTES) } {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    if let Err(code) = validate_batch(bytes) {
        return code;
    }
    let guard = runtime()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    match guard.as_ref() {
        Some(runtime) => send_with_timeout(runtime, bytes.to_vec()),
        None => PGR_ERR_STATE,
    }
}

unsafe fn copy_response(body: &[u8], response_utf8: *mut c_char, capacity: *mut usize) -> i32 {
    if capacity.is_null() {
        return PGR_ERR_INVALID_ARGUMENT;
    }
    let required = body.len() + 1;
    let available = *capacity;
    *capacity = required;
    if response_utf8.is_null() || available < required {
        return PGR_ERR_BUFFER_TOO_SMALL;
    }
    ptr::copy_nonoverlapping(body.as_ptr(), response_utf8.cast::<u8>(), body.len());
    *response_utf8.add(body.len()) = 0;
    PGR_OK
}

fn check_impl(
    request_utf8: *const c_char,
    request_len: usize,
    response_utf8: *mut c_char,
    response_capacity: *mut usize,
) -> i32 {
    let bytes = match unsafe { required_bytes(request_utf8, request_len, MAX_DECISION_BYTES) } {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    if let Err(code) = validate_decision(bytes) {
        return code;
    }
    let client = {
        let guard = runtime()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match guard.as_ref() {
            Some(runtime) => runtime.client.clone(),
            None => return PGR_ERR_STATE,
        }
    };
    match client.post("/agent/v1/decisions:check", bytes) {
        Ok(response)
            if (200..300).contains(&response.status) && valid_decision_response(&response.body) =>
        unsafe { copy_response(&response.body, response_utf8, response_capacity) },
        Ok(response) if (200..300).contains(&response.status) => PGR_ERR_RETRY,
        Ok(response) if (400..500).contains(&response.status) => PGR_ERR_AGENT_REJECTED,
        Ok(_) => PGR_ERR_RETRY,
        Err(ClientError::Timeout) => PGR_ERR_TIMEOUT,
        Err(_) => PGR_ERR_RETRY,
    }
}

fn action_request_impl(
    path: &str,
    request_utf8: *const c_char,
    request_len: usize,
    response_utf8: *mut c_char,
    response_capacity: *mut usize,
) -> i32 {
    let bytes = match unsafe { required_bytes(request_utf8, request_len, MAX_DECISION_BYTES) } {
        Ok(bytes) => bytes,
        Err(code) => return code,
    };
    if let Err(code) = validate_json_object(bytes) {
        return code;
    }
    let client = {
        let guard = runtime()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match guard.as_ref() {
            Some(runtime) => runtime.client.clone(),
            None => return PGR_ERR_STATE,
        }
    };
    match client.post(path, bytes) {
        Ok(response)
            if (200..300).contains(&response.status)
                && valid_action_response(path, &response.body) =>
        unsafe { copy_response(&response.body, response_utf8, response_capacity) },
        Ok(response) if (200..300).contains(&response.status) => PGR_ERR_RETRY,
        Ok(response) if (400..500).contains(&response.status) => PGR_ERR_AGENT_REJECTED,
        Ok(_) => PGR_ERR_RETRY,
        Err(ClientError::Timeout) => PGR_ERR_TIMEOUT,
        Err(_) => PGR_ERR_RETRY,
    }
}

fn flush_impl(timeout_ms: u32) -> i32 {
    if timeout_ms > 60_000 {
        return PGR_ERR_INVALID_ARGUMENT;
    }
    let pending = {
        let guard = runtime()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match guard.as_ref() {
            Some(runtime) => Arc::clone(&runtime.pending),
            None => return PGR_ERR_STATE,
        }
    };
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.into());
    let mut count = pending
        .count
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    while *count != 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return PGR_ERR_TIMEOUT;
        }
        let result = pending.changed.wait_timeout(count, remaining);
        let (next, wait) = result.unwrap_or_else(|poison| poison.into_inner());
        count = next;
        if wait.timed_out() && *count != 0 {
            return PGR_ERR_TIMEOUT;
        }
    }
    if pending.rejected.load(Ordering::Relaxed) != 0 {
        PGR_ERR_AGENT_REJECTED
    } else {
        PGR_OK
    }
}

fn shutdown_impl() {
    let mut sdk = runtime()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take();
    if let Some(ref runtime) = sdk {
        runtime.stopping.store(true, Ordering::Relaxed);
    }
    if let Some(worker) = sdk.as_mut().and_then(|runtime| runtime.worker.take()) {
        drop(sdk.take());
        let _ = worker.join();
    }
}

fn ffi_code(action: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(action)).unwrap_or(PGR_ERR_INTERNAL)
}

#[no_mangle]
pub extern "C" fn pgr_init(config: *const PgrConfigV1) -> i32 {
    ffi_code(|| init_impl(config))
}

#[no_mangle]
pub extern "C" fn pgr_emit_json(json_utf8: *const c_char, json_len: usize) -> i32 {
    ffi_code(|| emit_impl(json_utf8, json_len))
}

#[no_mangle]
pub extern "C" fn pgr_check_json(
    request_utf8: *const c_char,
    request_len: usize,
    response_utf8: *mut c_char,
    response_capacity: *mut usize,
) -> i32 {
    ffi_code(|| check_impl(request_utf8, request_len, response_utf8, response_capacity))
}

#[no_mangle]
pub extern "C" fn pgr_pull_actions(
    request_utf8: *const c_char,
    request_len: usize,
    response_utf8: *mut c_char,
    response_capacity: *mut usize,
) -> i32 {
    ffi_code(|| {
        action_request_impl(
            "/agent/v1/actions:pull",
            request_utf8,
            request_len,
            response_utf8,
            response_capacity,
        )
    })
}

#[no_mangle]
pub extern "C" fn pgr_ack_action(
    request_utf8: *const c_char,
    request_len: usize,
    response_utf8: *mut c_char,
    response_capacity: *mut usize,
) -> i32 {
    ffi_code(|| {
        action_request_impl(
            "/agent/v1/actions:ack",
            request_utf8,
            request_len,
            response_utf8,
            response_capacity,
        )
    })
}

#[no_mangle]
pub extern "C" fn pgr_flush(timeout_ms: u32) -> i32 {
    ffi_code(|| flush_impl(timeout_ms))
}

#[no_mangle]
pub extern "C" fn pgr_shutdown() {
    let _ = catch_unwind(AssertUnwindSafe(shutdown_impl));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn endpoint_accepts_loopback_or_https_gateway() {
        assert_eq!(
            parse_endpoint("http://127.0.0.1:17870").unwrap(),
            EndpointConfig::Local(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 17870))
        );
        assert_eq!(
            parse_endpoint("https://risk.example.com/sdk/v1/").unwrap(),
            EndpointConfig::Remote("https://risk.example.com/sdk/v1".to_string())
        );
        assert!(parse_endpoint("http://localhost:17870").is_err());
        assert!(parse_endpoint("http://192.168.1.8:17870").is_err());
        assert!(parse_endpoint("https://risk.example.com/").is_err());
        assert!(parse_endpoint("https://user@risk.example.com/sdk/v1").is_err());
        assert!(parse_endpoint("https://risk.example.com/sdk/v1?token=bad").is_err());
        assert!(parse_endpoint("http://127.0.0.1:0").is_err());
    }

    #[test]
    fn remote_paths_map_only_agent_v1_contract() {
        assert_eq!(
            remote_target("https://risk.example.com/sdk/v1", "/agent/v1/events:batch").unwrap(),
            "https://risk.example.com/sdk/v1/events:batch"
        );
        assert_eq!(
            remote_target("https://risk.example.com/sdk/v1", "/agent/v1/actions:pull").unwrap(),
            "https://risk.example.com/sdk/v1/actions:pull"
        );
        assert!(remote_target("https://risk.example.com/sdk/v1", "/other/path").is_err());
    }

    #[test]
    fn token_rejects_short_and_header_control_bytes() {
        assert!(valid_token("0123456789abcdef0123456789abcdef"));
        assert!(!valid_token("short"));
        assert!(!valid_token("0123456789abcdef0123456789abc\ndef"));
    }

    #[test]
    fn batch_requires_contract_envelope() {
        assert!(validate_batch(br#"{"schema_version":"1.0","producer":{},"events":[{}]}"#).is_ok());
        assert!(validate_batch(br#"{"schema_version":"1.0","events":[]}"#).is_err());
        assert!(validate_batch(b"not-json").is_err());
    }

    #[test]
    fn response_copy_reports_required_nul_terminated_size() {
        let body = br#"{"decision":"allow"}"#;
        let mut required = 1usize;
        let mut small = [0i8; 1];
        assert_eq!(
            unsafe { copy_response(body, small.as_mut_ptr(), &mut required) },
            PGR_ERR_BUFFER_TOO_SMALL
        );
        assert_eq!(required, body.len() + 1);

        let mut output = vec![0i8; required];
        let mut capacity = output.len();
        assert_eq!(
            unsafe { copy_response(body, output.as_mut_ptr(), &mut capacity) },
            PGR_OK
        );
        assert_eq!(capacity, required);
        assert_eq!(unsafe { CStr::from_ptr(output.as_ptr()) }.to_bytes(), body);
    }

    #[test]
    fn agent_responses_are_not_trusted_by_status_code_alone() {
        assert_eq!(
            batch_ack_rejected(br#"{"accepted":7,"duplicates":0,"rejected":[]}"#),
            Some(false)
        );
        assert_eq!(
            batch_ack_rejected(br#"{"accepted":6,"duplicates":0,"rejected":[{"event_id":"bad"}]}"#),
            Some(true)
        );
        assert_eq!(batch_ack_rejected(br#"{"ok":true}"#), None);
        assert!(valid_decision_response(
            br#"{"decision_id":"d1","mode":"shadow","decision":"allow"}"#
        ));
        assert!(!valid_decision_response(br#"{"decision":"allow"}"#));
    }

    #[test]
    fn http_client_sends_token_and_reads_body() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = match listener.local_addr().unwrap() {
            std::net::SocketAddr::V4(address) => address,
            _ => unreachable!(),
        };
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 256];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
            }
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert!(headers.contains("POST /agent/v1/decisions:check HTTP/1.1"));
            assert!(headers.contains("X-PGR-Local-Token: 0123456789abcdef0123456789abcdef"));
            while request.len() < header_end + content_length {
                let read = stream.read(&mut chunk).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{\"decision\":\"allow\"}")
                .unwrap();
        });
        let client = HttpClient {
            transport: ClientTransport::Local(address),
            token: Arc::from("0123456789abcdef0123456789abcdef"),
            timeout: Duration::from_secs(1),
        };
        let response = client.post("/agent/v1/decisions:check", b"{}").unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, br#"{"decision":"allow"}"#);
        server.join().unwrap();
    }
}
