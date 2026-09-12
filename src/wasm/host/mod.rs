//! Host-call dispatcher for WASI core-module guests.
//!
//! Core modules cannot use the component model's typed imports, so the host
//! exposes a single JSON-in/JSON-out entry point through the `xfetch` import
//! module:
//!
//! ```text
//! host_call(op_ptr, op_len, args_ptr, args_len) -> (len << 32) | ptr
//! ```
//!
//! The guest writes the operation name and a JSON argument object into its
//! linear memory, exports `xfetch_alloc(size) -> ptr` for the host to reserve
//! the response buffer, and reads the packed pointer/length from the return
//! value. Responses are always JSON objects:
//!
//! ```json
//! { "ok": true,  "value": { ... } }
//! { "ok": false, "error": { "kind": "denied", "message": "..." } }
//! ```
//!
//! Returning `0` means the host could not allocate the response at all.
//! Components bypass this layer and call the typed `host` interface directly;
//! both paths share the same capability policy and operation implementations.

pub mod exec;
pub mod http;

use crate::wasm::GuestKind;
use crate::wasm::policy::Policy;
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Host runtime protocol version reported to guests.
pub const PROTOCOL_VERSION: u32 = 1;

/// Environment variable that controls guest log verbosity.
pub const LOG_LEVEL_ENV: &str = "XFETCH_WASM_LOG_LEVEL";

/// Verbosity for guest `log` calls.
///
/// Ordered from least to most verbose so `allows` is a simple comparison.
/// The default is [`LogLevel::Warn`]: informational guest chatter stays out of
/// a normal fetch, while warnings and errors are still surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    /// Parses a case-insensitive level name.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "silent" => Some(Self::Off),
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" | "trace" => Some(Self::Debug),
            _ => None,
        }
    }

    /// Whether a message at `level` should be printed under this threshold.
    pub fn allows(self, level: LogLevel) -> bool {
        self != Self::Off && level <= self
    }
}

/// Guests carrying their own level names on the wire.
fn level_from_wire(level: &str) -> LogLevel {
    LogLevel::parse(level).unwrap_or(LogLevel::Info)
}

/// The configured log threshold, read once from the environment.
pub fn log_level() -> LogLevel {
    static LEVEL: OnceLock<LogLevel> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        std::env::var(LOG_LEVEL_ENV)
            .ok()
            .and_then(|value| LogLevel::parse(&value))
            .unwrap_or(LogLevel::Warn)
    })
}

/// Prints one guest log line when the configured threshold allows it.
///
/// Shared by the core-module JSON bridge and the typed component bindings.
pub fn log_message(guest: &str, level: &str, message: &str) {
    let requested = level_from_wire(level);
    if !log_level().allows(requested) {
        return;
    }
    eprintln!("[{}] [{}] {}", guest, requested.as_str(), message);
}

impl LogLevel {
    /// Canonical wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

/// Per-invocation host context: resolved policy, deadline and guest name.
#[derive(Debug, Clone)]
pub struct HostContext {
    pub policy: Policy,
    pub deadline: Instant,
    pub name: String,
    pub kind: GuestKind,
}

impl HostContext {
    /// Time left before the invocation deadline; zero when exhausted.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// Machine-readable failure category returned to guests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostErrorKind {
    /// The manifest does not allow the requested capability.
    Denied,
    /// The operation failed at runtime.
    Failed,
    /// The operation exceeded its deadline.
    Timeout,
    /// The payload exceeded the configured size limit.
    TooLarge,
    /// The host does not implement the requested operation.
    Unsupported,
}

impl HostErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::TooLarge => "too_large",
            Self::Unsupported => "unsupported",
        }
    }
}

/// A host-call failure.
#[derive(Debug, Clone)]
pub struct HostCallError {
    pub kind: HostErrorKind,
    pub message: String,
}

impl HostCallError {
    pub fn new(kind: HostErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(HostErrorKind::Denied, message)
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(HostErrorKind::Failed, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(HostErrorKind::Timeout, message)
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(HostErrorKind::TooLarge, message)
    }
}

/// Result of one host operation before JSON wrapping.
pub type HostCallResult = Result<Value, HostCallError>;

/// Dispatches one host call and returns the JSON response bytes.
///
/// `args` is the raw JSON argument object; an empty slice is treated as JSON
/// `null`.
pub fn dispatch(op: &str, args: &[u8], ctx: &HostContext) -> Vec<u8> {
    let parsed: Value = if args.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(args) {
            Ok(value) => value,
            Err(err) => {
                return error_json(HostCallError::failed(format!(
                    "host_call arguments are not valid JSON: {}",
                    err
                )));
            }
        }
    };

    match dispatch_value(op, &parsed, ctx) {
        Ok(value) => serde_json::to_vec(&json!({ "ok": true, "value": value }))
            .unwrap_or_else(|_| error_json(HostCallError::failed("failed to serialize response"))),
        Err(err) => error_json(err),
    }
}

/// Dispatches one host operation against parsed JSON arguments.
///
/// Shared by the core-module JSON bridge and the typed component bindings so
/// capabilities behave identically for both guest shapes.
pub fn dispatch_value(op: &str, args: &Value, ctx: &HostContext) -> HostCallResult {
    match op {
        "http" => http::request(args, ctx),
        "exec" => exec::run(args, ctx),
        "log" => {
            log(ctx, args);
            Ok(json!({}))
        }
        "version" => Ok(json!({
            "runtime": "wasmtime",
            "protocol": PROTOCOL_VERSION,
            "xfetch": env!("CARGO_PKG_VERSION"),
            "guest_kind": ctx.kind.as_str(),
        })),
        other => Err(HostCallError::new(
            HostErrorKind::Unsupported,
            format!("unknown host op '{}'", other),
        )),
    }
}

/// Serializes an error response.
fn error_json(err: HostCallError) -> Vec<u8> {
    json!({
        "ok": false,
        "error": { "kind": err.kind.as_str(), "message": err.message }
    })
    .to_string()
    .into_bytes()
}

/// Writes a guest log line to stderr, prefixed with the guest name.
fn log(ctx: &HostContext, args: &Value) {
    let level = args.get("level").and_then(Value::as_str).unwrap_or("info");
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    log_message(&ctx.name, level, message);
}

/// Reads a required string field.
pub(crate) fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, HostCallError> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HostCallError::failed(format!("missing '{}' argument", field)))
}

/// Decodes a base64 field (`None` and empty strings decode to no bytes).
pub(crate) fn parse_bytes(args: &Value, field: &str) -> Result<Option<Vec<u8>>, HostCallError> {
    use base64::Engine as _;

    match args.get(field).and_then(Value::as_str) {
        None | Some("") => Ok(None),
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map(Some)
            .map_err(|err| {
                HostCallError::failed(format!("invalid base64 in '{}': {}", field, err))
            }),
    }
}

/// Encodes bytes as base64 for the JSON wire format.
pub(crate) fn to_base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Resolves an optional per-call timeout, clamped by the invocation deadline.
pub(crate) fn parse_timeout(args: &Value, ctx: &HostContext) -> Result<Duration, HostCallError> {
    let remaining = ctx.remaining();
    if remaining.is_zero() {
        return Err(HostCallError::timeout("guest budget exhausted"));
    }

    let requested = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(remaining);

    Ok(requested.min(remaining))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::GuestKind;
    use crate::wasm::manifest::Manifest;
    use crate::wasm::policy::Policy;
    use serde_json::Value;

    fn context() -> HostContext {
        HostContext {
            policy: Policy::from_manifest(&Manifest::default()),
            deadline: Instant::now() + Duration::from_secs(10),
            name: "test".to_string(),
            kind: GuestKind::Plugin,
        }
    }

    fn parse_response(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("valid JSON response")
    }

    #[test]
    fn unknown_op_is_unsupported() {
        let response = parse_response(&dispatch("nope", b"{}", &context()));
        assert_eq!(response["ok"], Value::Bool(false));
        assert_eq!(response["error"]["kind"], "unsupported");
    }

    #[test]
    fn invalid_args_report_failure() {
        let response = parse_response(&dispatch("http", b"{not json", &context()));
        assert_eq!(response["error"]["kind"], "failed");
    }

    #[test]
    fn http_is_denied_without_allowlist() {
        let response = parse_response(&dispatch(
            "http",
            br#"{"url":"https://example.com"}"#,
            &context(),
        ));
        assert_eq!(response["error"]["kind"], "denied");
    }

    #[test]
    fn exec_is_denied_without_allowlist() {
        let response = parse_response(&dispatch("exec", br#"{"program":"curl"}"#, &context()));
        assert_eq!(response["error"]["kind"], "denied");
    }

    #[test]
    fn parses_log_levels_case_insensitively() {
        assert_eq!(LogLevel::parse("OFF"), Some(LogLevel::Off));
        assert_eq!(LogLevel::parse("error"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("Warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse(" info "), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("trace"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("nonsense"), None);
    }

    #[test]
    fn warn_threshold_hides_info_and_allows_errors() {
        let threshold = LogLevel::Warn;
        assert!(threshold.allows(LogLevel::Error));
        assert!(threshold.allows(LogLevel::Warn));
        assert!(!threshold.allows(LogLevel::Info));
        assert!(!threshold.allows(LogLevel::Debug));
    }

    #[test]
    fn off_threshold_hides_everything() {
        assert!(!LogLevel::Off.allows(LogLevel::Error));
        assert!(!LogLevel::Off.allows(LogLevel::Info));
    }

    #[test]
    fn unknown_wire_levels_are_treated_as_info() {
        assert_eq!(level_from_wire("banana"), LogLevel::Info);
    }

    #[test]
    fn version_op_reports_protocol() {
        let response = parse_response(&dispatch("version", b"{}", &context()));
        assert_eq!(response["ok"], Value::Bool(true));
        assert_eq!(response["value"]["protocol"], PROTOCOL_VERSION);
    }

    #[test]
    fn base64_round_trip() {
        assert_eq!(to_base64(b"hello"), "aGVsbG8=");
        let decoded =
            parse_bytes(&json!({ "body_base64": "aGVsbG8=" }), "body_base64").expect("decode");
        assert_eq!(decoded.as_deref(), Some(&b"hello"[..]));
    }
}
