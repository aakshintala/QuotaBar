#[cfg(any(target_os = "linux", test))]
use std::fs::File;
#[cfg(any(target_os = "linux", test))]
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", test))]
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderValue, RETRY_AFTER};
use reqwest::{Client, Url};
#[cfg(any(target_os = "linux", test))]
use serde::Deserialize;

use crate::feed::ErrorCode;
use crate::providers::http::{build_client, parse_retry_after, read_bounded_body};
#[cfg(any(target_os = "macos", test))]
use crate::providers::keychain::{CredentialReader, KeychainReadError, KeychainTarget};
use crate::quota::{Observation, Quota};
use crate::scheduler::AttemptOutcome;

const USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_JWT_SUB_BYTES: usize = 512;

/// Read-only Cursor usage probe. Its production endpoint and credential source
/// are fixed for the target OS; tests can substitute only a loopback endpoint
/// and synthetic credential source.
pub struct CursorHttpProbe {
    client: Client,
    source: CredentialSource,
    usage_url: Url,
}

enum CredentialSource {
    #[cfg(any(target_os = "linux", test))]
    File(PathBuf),
    #[cfg(any(target_os = "macos", test))]
    Keychain(Arc<dyn CredentialReader + Send + Sync>),
}

/// Access tokens are kept opaque and cleared when dropped. This type does not
/// implement `Debug` and is never serialized.
struct CursorToken(Vec<u8>);

impl Drop for CursorToken {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl CursorHttpProbe {
    #[cfg(test)]
    fn for_loopback_test(usage_url: Url, source: CredentialSource, timeout: Duration) -> Self {
        assert_eq!(usage_url.scheme(), "http");
        assert_eq!(usage_url.host_str(), Some("127.0.0.1"));
        Self {
            client: build_client(timeout, timeout).expect("test HTTP client builds"),
            source,
            usage_url,
        }
    }

    /// Construct a production probe rooted at the service's explicit HOME.
    /// This builds a client and stores paths only; it does not read credentials
    /// or contact Cursor.
    pub fn new(home: PathBuf) -> Result<Self, ErrorCode> {
        let client = build_client(REQUEST_TIMEOUT, CONNECT_TIMEOUT)?;
        Self::with_client(home, client)
    }

    /// Construct with a caller-owned shared client while keeping the provider
    /// URL and OS-selected credential source fixed.
    pub(crate) fn with_client(home: PathBuf, client: Client) -> Result<Self, ErrorCode> {
        let usage_url = Url::parse(USAGE_URL).map_err(|_| ErrorCode::ProviderError)?;
        let source = production_source(&home);
        Ok(Self {
            client,
            source,
            usage_url,
        })
    }

    /// Perform one bounded request, with at most one read-only credential
    /// reread and one HTTP retry after a rejected token.
    pub async fn probe_at(&self, captured_at: DateTime<Utc>) -> AttemptOutcome {
        let token = match self.read_token(Utc::now()) {
            Ok(token) => token,
            Err(error) => return AttemptOutcome::failure(error),
        };

        let (status, first) = self.send_once(&token, captured_at).await;
        if status != Some(401) {
            return first;
        }

        // A 401 can mean the CLI rotated its token since the first read. Read
        // the source once, but never refresh or write Cursor credentials.
        let replacement = match self.read_token(Utc::now()) {
            Ok(token) => token,
            Err(error) => {
                return AttemptOutcome {
                    result: Err(error),
                    retry_after: first.retry_after,
                };
            }
        };
        if replacement.0 == token.0 {
            return first;
        }

        // A future server cooldown is authoritative even if the CLI token
        // changed between the two reads. Do not issue another HTTP request.
        if first.retry_after.is_some() {
            return first;
        }

        self.send_once(&replacement, captured_at).await.1
    }

    fn read_token(&self, now: DateTime<Utc>) -> Result<CursorToken, ErrorCode> {
        let mut token = match &self.source {
            #[cfg(any(target_os = "linux", test))]
            CredentialSource::File(path) => read_file_token(path)?,
            #[cfg(any(target_os = "macos", test))]
            CredentialSource::Keychain(reader) => {
                let secret = reader
                    .read(KeychainTarget::CursorAccess)
                    .map_err(map_keychain_error)?;
                let value = std::str::from_utf8(secret.expose())
                    .map_err(|_| ErrorCode::CredentialsMissing)?;
                CursorToken(value.as_bytes().to_vec())
            }
        };

        match decode_jwt(&token.0) {
            Ok((_, Some(exp))) if exp <= now.timestamp() as f64 => {
                token.0.fill(0);
                Err(ErrorCode::LoginExpired)
            }
            Ok(_) => Ok(token),
            Err(()) => {
                token.0.fill(0);
                Err(ErrorCode::CredentialsMissing)
            }
        }
    }

    async fn send_once(
        &self,
        token: &CursorToken,
        captured_at: DateTime<Utc>,
    ) -> (Option<u16>, AttemptOutcome) {
        let cookie = match cookie_header(token) {
            Some(cookie) => cookie,
            None => return (None, AttemptOutcome::failure(ErrorCode::CredentialsMissing)),
        };
        let response = match self
            .client
            .get(self.usage_url.clone())
            .header(COOKIE, cookie)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return (None, AttemptOutcome::failure(ErrorCode::NetworkError)),
        };

        // reqwest resolves `send` at response headers. Sample now, before
        // status handling or body collection, so Retry-After survives errors.
        let headers_received_at = Utc::now();
        let status = response.status();
        let status_code = status.as_u16();
        let retry_after =
            parse_retry_after(response.headers().get(RETRY_AFTER), headers_received_at);
        if status_code != 200 {
            let error = match status_code {
                401 => ErrorCode::LoginExpired,
                403 => ErrorCode::ProviderError,
                429 => ErrorCode::RateLimited,
                _ => ErrorCode::ProviderError,
            };
            return (
                Some(status_code),
                AttemptOutcome {
                    result: Err(error),
                    retry_after,
                },
            );
        }

        let body = match read_bounded_body(response, MAX_RESPONSE_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                return (
                    Some(status_code),
                    AttemptOutcome {
                        result: Err(error),
                        retry_after,
                    },
                );
            }
        };
        match parse_usage(&body, captured_at) {
            Ok(observation) => (
                Some(status_code),
                AttemptOutcome {
                    result: Ok(observation),
                    retry_after,
                },
            ),
            Err(ParseError::Malformed) => (
                Some(status_code),
                AttemptOutcome {
                    result: Err(ErrorCode::ProviderError),
                    retry_after,
                },
            ),
        }
    }
}

fn production_source(home: &Path) -> CredentialSource {
    #[cfg(target_os = "macos")]
    {
        let _ = home;
        CredentialSource::Keychain(Arc::new(crate::providers::keychain::NativeKeychainReader))
    }
    #[cfg(target_os = "linux")]
    {
        CredentialSource::File(cursor_auth_path(home))
    }
}

#[cfg(any(target_os = "linux", test))]
fn cursor_auth_path(home: &Path) -> PathBuf {
    home.join(".config/cursor/auth.json")
}

#[cfg(any(target_os = "macos", test))]
fn map_keychain_error(error: KeychainReadError) -> ErrorCode {
    match error {
        KeychainReadError::ItemNotFound => ErrorCode::CredentialsMissing,
        KeychainReadError::InteractionUnavailable => ErrorCode::KeychainBlocked,
        KeychainReadError::CredentialTooLarge | KeychainReadError::ReadFailed => {
            ErrorCode::ProviderError
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn read_file_token(path: &Path) -> Result<CursorToken, ErrorCode> {
    let mut file = File::open(path).map_err(|_| ErrorCode::CredentialsMissing)?;
    let mut bytes = Vec::new();
    if file
        .by_ref()
        .take((MAX_CREDENTIAL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        bytes.fill(0);
        return Err(ErrorCode::CredentialsMissing);
    }
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        bytes.fill(0);
        return Err(ErrorCode::CredentialsMissing);
    }

    let mut credentials: CursorAuthFile = match serde_json::from_slice(&bytes) {
        Ok(credentials) => credentials,
        Err(_) => {
            bytes.fill(0);
            return Err(ErrorCode::CredentialsMissing);
        }
    };
    bytes.fill(0);
    let Some(token) = credentials.access_token.take() else {
        return Err(ErrorCode::CredentialsMissing);
    };
    let token = CursorToken(token.into_bytes());
    // JWT validation is shared with the Keychain path in `read_token`.
    Ok(token)
}

#[cfg(any(target_os = "linux", test))]
#[derive(Deserialize)]
struct CursorAuthFile {
    #[serde(rename = "accessToken")]
    access_token: Option<SecretString>,
}

/// Deserialized credential strings clear their allocation on every path,
/// including malformed-token handling.
#[cfg(any(target_os = "linux", test))]
struct SecretString(String);

#[cfg(any(target_os = "linux", test))]
impl SecretString {
    fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0).into_bytes()
    }
}

#[cfg(any(target_os = "linux", test))]
impl Drop for SecretString {
    fn drop(&mut self) {
        // `String::as_mut_vec` is unsafe because callers must preserve UTF-8;
        // zeroing all bytes and leaving the value to be dropped is safe here.
        unsafe { self.0.as_mut_vec().fill(0) };
    }
}

#[cfg(any(target_os = "linux", test))]
impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

fn decode_jwt(token: &[u8]) -> Result<(String, Option<f64>), ()> {
    if token.is_empty() || token.len() > MAX_CREDENTIAL_BYTES {
        return Err(());
    }
    let token = std::str::from_utf8(token).map_err(|_| ())?;
    let mut parts = token.split('.');
    let header = parts.next().ok_or(())?;
    let payload = parts.next().ok_or(())?;
    let signature = parts.next().ok_or(())?;
    if parts.next().is_some()
        || !valid_jwt_segment(header)
        || !valid_jwt_segment(payload)
        || !valid_jwt_segment(signature)
        || decode_base64url(header).is_none()
        || decode_base64url(payload).is_none()
        || decode_base64url(signature).is_none()
    {
        return Err(());
    }

    let payload_bytes = decode_base64url(payload).ok_or(())?;
    let claims: serde_json::Value = serde_json::from_slice(&payload_bytes).map_err(|_| ())?;
    let claims = claims.as_object().ok_or(())?;
    let subject = claims
        .get("sub")
        .and_then(serde_json::Value::as_str)
        .ok_or(())?;
    if !valid_subject(subject) {
        return Err(());
    }
    let exp = match claims.get("exp") {
        None => None,
        Some(value) => {
            let exp = value.as_f64().filter(|value| value.is_finite()).ok_or(())?;
            Some(exp)
        }
    };
    Ok((subject.to_owned(), exp))
}

fn valid_jwt_segment(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    let mut padding_started = false;
    let mut padding_count = 0;
    for byte in segment.bytes() {
        if byte == b'=' {
            padding_started = true;
            padding_count += 1;
            if padding_count > 2 {
                return false;
            }
        } else if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            if padding_started {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}

fn decode_base64url(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| URL_SAFE.decode(value))
        .ok()
}

fn valid_subject(subject: &str) -> bool {
    !subject.is_empty()
        && subject.len() <= MAX_JWT_SUB_BYTES
        && subject
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'|'))
}

fn cookie_header(token: &CursorToken) -> Option<HeaderValue> {
    let (subject, _) = decode_jwt(&token.0).ok()?;
    let token = std::str::from_utf8(&token.0).ok()?;
    let cookie = format!("WorkosCursorSessionToken={subject}::{token}");
    let mut header = HeaderValue::from_bytes(cookie.as_bytes()).ok()?;
    header.set_sensitive(true);
    Some(header)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParseError {
    Malformed,
}

/// Parse Cursor's observed usage-summary response without retaining provider
/// text. Cursor billing-cycle meters intentionally have no inferred window
/// duration, so the shared status calculation uses absolute thresholds.
fn parse_usage(input: &[u8], captured_at: DateTime<Utc>) -> Result<Observation, ParseError> {
    if input.len() > MAX_RESPONSE_BYTES {
        return Err(ParseError::Malformed);
    }
    let root: serde_json::Value =
        serde_json::from_slice(input).map_err(|_| ParseError::Malformed)?;
    let object = root.as_object().ok_or(ParseError::Malformed)?;
    let membership = object
        .get("membershipType")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let tier = if membership.is_empty() {
        None
    } else {
        Some(membership.to_ascii_uppercase())
    };
    let resets_at = parse_reset(object.get("billingCycleEnd"))?;
    let individual = object
        .get("individualUsage")
        .and_then(serde_json::Value::as_object);
    let team = (object.get("limitType").and_then(serde_json::Value::as_str) == Some("team"))
        .then(|| {
            object
                .get("teamUsage")
                .and_then(serde_json::Value::as_object)
        })
        .flatten();

    let mut quotas = Vec::new();
    if let Some(quota) = parse_meter(
        individual.and_then(|value| value.get("plan")),
        "time:Monthly",
        "Monthly",
        resets_at,
    )? {
        quotas.push(quota);
    }
    if let Some(quota) = parse_meter(
        individual.and_then(|value| value.get("onDemand")),
        "time:On-Demand",
        "On-Demand",
        resets_at,
    )? {
        quotas.push(quota);
    }
    if let Some(quota) = parse_meter(
        team.and_then(|value| value.get("onDemand")),
        "time:Team",
        "Team",
        resets_at,
    )? {
        quotas.push(quota);
    }

    if object
        .get("isUnlimited")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        quotas.push(Quota {
            key: "time:Monthly".to_owned(),
            label: "Monthly".to_owned(),
            percent_remaining: Some(100.0),
            balance_remaining: None,
            balance_used: None,
            balance_cap: None,
            balance_unit: None,
            units_used: None,
            units_limit: None,
            resets_at: None,
            window_duration_seconds: None,
        });
    }
    if quotas.is_empty() {
        return Err(ParseError::Malformed);
    }

    Ok(Observation {
        captured_at,
        tier,
        quotas,
    })
}

fn parse_reset(value: Option<&serde_json::Value>) -> Result<Option<DateTime<Utc>>, ParseError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => DateTime::parse_from_rfc3339(value)
            .map(|value| Some(value.with_timezone(&Utc)))
            .map_err(|_| ParseError::Malformed),
        Some(_) => Err(ParseError::Malformed),
    }
}

fn parse_meter(
    value: Option<&serde_json::Value>,
    key: &str,
    label: &str,
    resets_at: Option<DateTime<Utc>>,
) -> Result<Option<Quota>, ParseError> {
    let Some(meter) = value.and_then(serde_json::Value::as_object) else {
        return Ok(None);
    };
    match meter.get("enabled") {
        Some(serde_json::Value::Bool(true)) => {}
        Some(serde_json::Value::Bool(false)) | None => return Ok(None),
        Some(_) => return Err(ParseError::Malformed),
    }

    let used = parse_count(meter.get("used"))?.unwrap_or(0);
    let base_limit = parse_count(meter.get("limit"))?.unwrap_or(0);
    let bonus_total = meter
        .get("breakdown")
        .and_then(serde_json::Value::as_object)
        .and_then(|value| value.get("total"));
    let breakdown_total = parse_count(bonus_total)?.unwrap_or(0);
    let limit = base_limit.max(breakdown_total);
    if limit <= 0 {
        return Ok(None);
    }

    let (percent_remaining, effective_used) = match meter.get("totalPercentUsed") {
        Some(value) => {
            let used_percent = value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or(ParseError::Malformed)?;
            let calculated_used = used_percent * limit as f64 / 100.0;
            if !calculated_used.is_finite()
                || calculated_used < i64::MIN as f64
                || calculated_used >= i64::MAX as f64
            {
                return Err(ParseError::Malformed);
            }
            (
                (100.0 - used_percent).max(0.0),
                calculated_used.round() as i64,
            )
        }
        None => {
            let remaining = (limit as f64 - used as f64) / limit as f64 * 100.0;
            if !remaining.is_finite() {
                return Err(ParseError::Malformed);
            }
            (remaining.max(0.0), used)
        }
    };
    if !percent_remaining.is_finite() {
        return Err(ParseError::Malformed);
    }

    Ok(Some(Quota {
        key: key.to_owned(),
        label: label.to_owned(),
        percent_remaining: Some(percent_remaining),
        balance_remaining: None,
        balance_used: None,
        balance_cap: None,
        balance_unit: None,
        units_used: Some(effective_used),
        units_limit: Some(limit),
        resets_at,
        window_duration_seconds: None,
    }))
}

fn parse_count(value: Option<&serde_json::Value>) -> Result<Option<i64>, ParseError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => {
            if let Some(integer) = value.as_i64() {
                return Ok(Some(integer));
            }
            if let Some(unsigned) = value.as_u64() {
                return i64::try_from(unsigned)
                    .map(Some)
                    .map_err(|_| ParseError::Malformed);
            }
            let float = value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or(ParseError::Malformed)?;
            if float < i64::MIN as f64 || float >= i64::MAX as f64 {
                return Err(ParseError::Malformed);
            }
            Ok(Some(float as i64))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONNECT_TIMEOUT, CredentialSource, CursorHttpProbe, MAX_CREDENTIAL_BYTES,
        MAX_RESPONSE_BYTES, ParseError, REQUEST_TIMEOUT, SecretString, cursor_auth_path,
        decode_jwt, parse_count, parse_usage, production_source, read_file_token,
    };
    use crate::feed::ErrorCode;
    use crate::providers::keychain::{
        CredentialReader, KeychainReadError, KeychainTarget, SecretBytes,
    };
    use base64::Engine as _;
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    use reqwest::Url;
    use std::collections::VecDeque;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    const USAGE_JSON: &str = r#"{
        "membershipType":"pro",
        "billingCycleEnd":"2027-03-06T03:34:49.000Z",
        "limitType":"user",
        "isUnlimited":false,
        "individualUsage":{
            "plan":{"enabled":true,"used":326,"limit":40000,"remaining":39674,"breakdown":{"included":40000,"bonus":0,"total":40000},"totalPercentUsed":0.815},
            "onDemand":{"enabled":false,"used":0,"limit":null,"remaining":null}
        }
    }"#;

    static NEXT_HOME: AtomicUsize = AtomicUsize::new(0);

    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Self {
            let index = NEXT_HOME.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("quotabard-cursor-{}-{index}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn credential_path(&self) -> PathBuf {
            cursor_auth_path(self.path())
        }

        fn write_token(&self, token: &str) {
            let path = self.credential_path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, auth_json(token)).unwrap();
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone)]
    enum FakeRead {
        Bytes(Vec<u8>),
        Error(KeychainReadError),
    }

    struct FakeReader {
        reads: AtomicUsize,
        replies: Mutex<VecDeque<FakeRead>>,
    }

    impl FakeReader {
        fn new(replies: impl IntoIterator<Item = FakeRead>) -> Self {
            Self {
                reads: AtomicUsize::new(0),
                replies: Mutex::new(replies.into_iter().collect()),
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    impl CredentialReader for FakeReader {
        fn read(&self, target: KeychainTarget) -> Result<SecretBytes, KeychainReadError> {
            assert!(target == KeychainTarget::CursorAccess);
            self.reads.fetch_add(1, Ordering::SeqCst);
            match self.replies.lock().unwrap().pop_front().unwrap() {
                FakeRead::Bytes(value) => Ok(SecretBytes::from_bytes(value)),
                FakeRead::Error(error) => Err(error),
            }
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn jwt(sub: &str, exp: Option<f64>) -> String {
        let mut payload = serde_json::Map::new();
        payload.insert("sub".into(), serde_json::Value::String(sub.into()));
        if let Some(exp) = exp {
            payload.insert("exp".into(), serde_json::json!(exp));
        }
        let payload = serde_json::to_vec(&serde_json::Value::Object(payload)).unwrap();
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("eyJhbGciOiJIUzI1NiJ9.{payload}.c2ln")
    }

    fn auth_json(token: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "accessToken": token })).unwrap()
    }

    fn test_probe(home: &TestHome, endpoint: Url) -> CursorHttpProbe {
        CursorHttpProbe::for_loopback_test(
            endpoint,
            CredentialSource::File(home.credential_path()),
            Duration::from_secs(2),
        )
    }

    fn keychain_probe(_home: &TestHome, endpoint: Url, reader: Arc<FakeReader>) -> CursorHttpProbe {
        CursorHttpProbe::for_loopback_test(
            endpoint,
            CredentialSource::Keychain(reader),
            Duration::from_secs(2),
        )
    }

    fn listen() -> (TcpListener, Url) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = TcpListener::from_std(listener).unwrap();
        let address = listener.local_addr().unwrap();
        (
            listener,
            Url::parse(&format!("http://{address}/usage")).unwrap(),
        )
    }

    async fn request_headers(stream: TcpStream) -> (String, TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut output = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            output.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        (output, reader.into_inner())
    }

    async fn respond(listener: TcpListener, response: Vec<u8>) -> String {
        let (stream, _) = listener.accept().await.unwrap();
        let (headers, mut stream) = request_headers(stream).await;
        stream.write_all(&response).await.unwrap();
        headers
    }

    fn response(status: u16, extra_headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status} test\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn response_with_length(
        status: u16,
        extra_headers: &str,
        length: usize,
        body: &[u8],
    ) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status} test\r\n{extra_headers}Content-Length: {length}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn header_value(headers: &str, name: &str) -> Option<String> {
        headers.lines().find_map(|line| {
            let (candidate, value) = line.split_once(':')?;
            candidate
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

    #[test]
    fn production_credential_source_is_os_specific_and_uses_only_cli_auth_path() {
        let home = Path::new("/synthetic-home");
        assert_eq!(
            cursor_auth_path(home),
            home.join(".config/cursor/auth.json")
        );
        #[cfg(target_os = "linux")]
        assert!(
            matches!(production_source(home), CredentialSource::File(path) if path == home.join(".config/cursor/auth.json"))
        );
        #[cfg(target_os = "macos")]
        assert!(matches!(
            production_source(home),
            CredentialSource::Keychain(_)
        ));
    }

    #[test]
    fn linux_file_loader_is_bounded_and_rejects_missing_malformed_and_control_tokens() {
        let home = TestHome::new();
        assert!(matches!(
            read_file_token(&home.credential_path()),
            Err(ErrorCode::CredentialsMissing)
        ));

        let path = home.credential_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for contents in [
            b"not-json".as_slice(),
            br#"{}"#.as_slice(),
            br#"{"accessToken":""}"#.as_slice(),
            br#"{"accessToken":"not a jwt"}"#.as_slice(),
            br#"{"accessToken":"abc\r\nX-Evil:yes"}"#.as_slice(),
        ] {
            fs::write(&path, contents).unwrap();
            let result = read_file_token(&path).and_then(|token| {
                decode_jwt(&token.0)
                    .map(|_| token)
                    .map_err(|_| ErrorCode::CredentialsMissing)
            });
            assert!(matches!(result, Err(ErrorCode::CredentialsMissing)));
        }

        fs::write(&path, vec![b' '; MAX_CREDENTIAL_BYTES + 1]).unwrap();
        assert!(matches!(
            read_file_token(&path),
            Err(ErrorCode::CredentialsMissing)
        ));
    }

    #[test]
    fn valid_jwt_claims_require_safe_subjects_and_finite_expiration() {
        let valid = jwt("github|user_01ABC", Some(1_900_000_000.5));
        let (sub, exp) = decode_jwt(valid.as_bytes()).unwrap();
        assert_eq!(sub, "github|user_01ABC");
        assert_eq!(exp, Some(1_900_000_000.5));

        for sub in [
            "",
            "bad;cookie",
            "bad::claim",
            "bad,claim",
            "bad\r\nX:y",
            "has space",
        ] {
            assert!(
                decode_jwt(jwt(sub, None).as_bytes()).is_err(),
                "accepted subject {sub:?}"
            );
        }
        for token in [
            "not-a-jwt",
            "a.b.c.d",
            "a.!!!!.c",
            "a..c",
            "a.eyJzdWIiOiJ1In0===.c",
            "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1IiwiZXhwIjoibm90LW51bWJlciJ9.sig",
            "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1IiwiZXhwIjo5ZTk5OX0.sig",
        ] {
            assert!(
                decode_jwt(token.as_bytes()).is_err(),
                "accepted invalid JWT {token:?}"
            );
        }
    }

    #[test]
    fn count_parser_accepts_large_integer_and_integral_or_fractional_json_numbers_safely() {
        assert_eq!(
            parse_count(Some(&serde_json::json!(9_007_199_254_740_991_i64))).unwrap(),
            Some(9_007_199_254_740_991)
        );
        assert_eq!(
            parse_count(Some(&serde_json::json!(123.9))).unwrap(),
            Some(123)
        );
        assert!(parse_count(Some(&serde_json::json!(1e300))).is_err());
        assert!(parse_count(Some(&serde_json::json!(u64::MAX))).is_err());
    }

    #[test]
    fn parses_bonus_total_percent_team_counts_and_unlimited_without_invented_resets() {
        let input = br#"{
          "membershipType":"enterprise","limitType":"team","isUnlimited":true,
          "billingCycleEnd":"2027-03-06T03:34:49.125Z",
          "individualUsage":{"plan":{"enabled":true,"used":2000,"limit":2000,"breakdown":{"included":2000,"bonus":7770,"total":9770},"totalPercentUsed":28.32},"onDemand":{"enabled":true,"used":25,"limit":100}},
          "teamUsage":{"onDemand":{"enabled":true,"used":0,"limit":10000}}
        }"#;
        let observation = parse_usage(input, now()).unwrap();
        assert_eq!(observation.tier.as_deref(), Some("ENTERPRISE"));
        assert_eq!(observation.quotas.len(), 4);
        let monthly = &observation.quotas[0];
        assert_eq!(monthly.key, "time:Monthly");
        assert_eq!(monthly.units_limit, Some(9_770));
        assert_eq!(monthly.units_used, Some(2_767));
        assert!((monthly.percent_remaining.unwrap() - 71.68).abs() < 0.01);
        assert_eq!(monthly.resets_at.unwrap().timestamp(), 1_804_304_089);
        assert_eq!(monthly.window_duration_seconds, None);
        assert_eq!(observation.quotas[1].label, "On-Demand");
        assert_eq!(observation.quotas[1].units_used, Some(25));
        assert_eq!(observation.quotas[2].label, "Team");
        assert_eq!(observation.quotas[2].units_limit, Some(10_000));
        assert_eq!(observation.quotas[3].label, "Monthly");
        assert_eq!(observation.quotas[3].percent_remaining, Some(100.0));
        assert_eq!(observation.quotas[3].resets_at, None);
        assert_eq!(observation.quotas[3].units_limit, None);
    }

    #[test]
    fn parser_falls_back_to_used_over_capacity_and_ignores_disabled_and_non_team_meters() {
        let input = br#"{
          "membershipType":"pro","limitType":"user","billingCycleEnd":"2027-03-06T03:34:49Z",
          "individualUsage":{"plan":{"enabled":true,"used":550,"limit":500},"onDemand":{"enabled":false,"used":2,"limit":5}},
          "teamUsage":{"onDemand":{"enabled":true,"used":1,"limit":10}}
        }"#;
        let observation = parse_usage(input, now()).unwrap();
        assert_eq!(observation.quotas.len(), 1);
        assert_eq!(observation.quotas[0].percent_remaining, Some(0.0));
        assert_eq!(observation.quotas[0].units_used, Some(550));
        assert_eq!(observation.quotas[0].units_limit, Some(500));

        let double_counts =
            br#"{"individualUsage":{"plan":{"enabled":true,"used":10.0,"limit":20.0}}}"#;
        assert_eq!(
            parse_usage(double_counts, now()).unwrap().quotas[0].units_used,
            Some(10)
        );
    }

    #[test]
    fn parser_rejects_malformed_missing_empty_and_non_finite_quota_data() {
        for input in [
            b"not-json".as_slice(),
            br#"{}"#.as_slice(),
            br#"{"isUnlimited":false,"individualUsage":{"plan":{"enabled":true,"used":1,"limit":0}}}"#.as_slice(),
            br#"{"individualUsage":{"plan":{"enabled":true,"used":1,"limit":5,"totalPercentUsed":1e999}}}"#.as_slice(),
            br#"{"individualUsage":{"plan":{"enabled":true,"used":1,"limit":5}},"billingCycleEnd":"tomorrow"}"#.as_slice(),
            br#"{"individualUsage":{"plan":{"enabled":true,"used":1,"limit":5}},"billingCycleEnd":5}"#.as_slice(),
            br#"{"individualUsage":{"plan":{"enabled":true,"used":1e999,"limit":5}}}"#.as_slice(),
        ] {
            assert_eq!(parse_usage(input, now()), Err(ParseError::Malformed));
        }
        assert_eq!(
            parse_usage(&vec![b' '; MAX_RESPONSE_BYTES + 1], now()),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn fake_keychain_reader_covers_success_expired_and_blocked_without_native_access() {
        let home = TestHome::new();
        let valid = jwt(
            "user_synthetic",
            Some(Utc::now().timestamp() as f64 + 3600.0),
        );
        let reader = Arc::new(FakeReader::new([FakeRead::Bytes(
            valid.as_bytes().to_vec(),
        )]));
        let probe = keychain_probe(
            &home,
            Url::parse("http://127.0.0.1:1/usage").unwrap(),
            Arc::clone(&reader),
        );
        assert!(probe.read_token(Utc::now()).is_ok());
        assert_eq!(reader.reads(), 1);

        let expired = jwt("user_synthetic", Some(Utc::now().timestamp() as f64 - 60.0));
        let reader = Arc::new(FakeReader::new([FakeRead::Bytes(expired.into_bytes())]));
        let probe = keychain_probe(
            &home,
            Url::parse("http://127.0.0.1:1/usage").unwrap(),
            Arc::clone(&reader),
        );
        assert!(matches!(
            probe.read_token(Utc::now()),
            Err(ErrorCode::LoginExpired)
        ));
        assert_eq!(reader.reads(), 1);

        let reader = Arc::new(FakeReader::new([FakeRead::Error(
            KeychainReadError::InteractionUnavailable,
        )]));
        let probe = keychain_probe(
            &home,
            Url::parse("http://127.0.0.1:1/usage").unwrap(),
            Arc::clone(&reader),
        );
        assert!(matches!(
            probe.read_token(Utc::now()),
            Err(ErrorCode::KeychainBlocked)
        ));
        assert_eq!(reader.reads(), 1);
    }

    #[test]
    fn credential_store_errors_map_to_safe_existing_codes() {
        assert_eq!(
            super::map_keychain_error(KeychainReadError::ItemNotFound),
            ErrorCode::CredentialsMissing
        );
        assert_eq!(
            super::map_keychain_error(KeychainReadError::InteractionUnavailable),
            ErrorCode::KeychainBlocked
        );
        assert_eq!(
            super::map_keychain_error(KeychainReadError::CredentialTooLarge),
            ErrorCode::ProviderError
        );
        assert_eq!(
            super::map_keychain_error(KeychainReadError::ReadFailed),
            ErrorCode::ProviderError
        );
        let wrapped = SecretString("synthetic".to_owned());
        assert_eq!(wrapped.into_bytes(), b"synthetic");
    }

    #[tokio::test]
    async fn success_sends_exact_cookie_to_loopback_and_parses_snapshot() {
        let home = TestHome::new();
        let token = jwt(
            "github|user_ABC",
            Some(Utc::now().timestamp() as f64 + 3600.0),
        );
        home.write_token(&token);
        let (listener, endpoint) = listen();
        let server = tokio::spawn(respond(
            listener,
            response(
                200,
                "Content-Type: application/json\r\n",
                USAGE_JSON.as_bytes(),
            ),
        ));

        let result = test_probe(&home, endpoint).probe_at(now()).await;
        let headers = server.await.unwrap();
        assert!(result.result.is_ok());
        assert_eq!(result.retry_after, None);
        let observation = result.result.unwrap();
        assert_eq!(observation.quotas.len(), 1);
        assert_eq!(observation.tier.as_deref(), Some("PRO"));
        assert_eq!(
            header_value(&headers, "cookie").as_deref(),
            Some(format!("WorkosCursorSessionToken=github|user_ABC::{token}").as_str())
        );
        assert_eq!(
            header_value(&headers, "content-type").as_deref(),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn a_401_rereads_a_rotated_cli_file_and_retries_exactly_once() {
        let home = TestHome::new();
        let old = jwt("user_old", Some(Utc::now().timestamp() as f64 + 3600.0));
        let new = jwt("user_new", Some(Utc::now().timestamp() as f64 + 3600.0));
        let new_for_server = new.clone();
        home.write_token(&old);
        let credential_path = home.credential_path();
        let (listener, endpoint) = listen();
        let server = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            let (first_headers, mut first) = request_headers(first).await;
            first
                .write_all(&response(401, "", b"ignored"))
                .await
                .unwrap();
            fs::write(&credential_path, auth_json(&new_for_server)).unwrap();
            let (second, _) = listener.accept().await.unwrap();
            let (second_headers, mut second) = request_headers(second).await;
            second
                .write_all(&response(200, "", USAGE_JSON.as_bytes()))
                .await
                .unwrap();
            (first_headers, second_headers)
        });

        let result = test_probe(&home, endpoint).probe_at(now()).await;
        let (first, second) = server.await.unwrap();
        assert!(result.result.is_ok());
        assert_eq!(
            header_value(&first, "cookie").unwrap(),
            format!("WorkosCursorSessionToken=user_old::{old}")
        );
        assert_eq!(
            header_value(&second, "cookie").unwrap(),
            format!("WorkosCursorSessionToken=user_new::{new}")
        );
    }

    #[tokio::test]
    async fn a_401_with_unchanged_credentials_does_not_loop() {
        let home = TestHome::new();
        let token = jwt("user_same", Some(Utc::now().timestamp() as f64 + 3600.0));
        home.write_token(&token);
        let (listener, endpoint) = listen();
        let server = tokio::spawn(respond(listener, response(401, "", b"ignored")));

        let outcome = test_probe(&home, endpoint).probe_at(now()).await;
        let headers = server.await.unwrap();
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::LoginExpired);
        assert!(header_value(&headers, "cookie").is_some());
    }

    #[tokio::test]
    async fn future_retry_after_on_401_is_kept_and_prevents_retry_even_after_token_rotation() {
        let home = TestHome::new();
        let old = jwt("user_old", Some(Utc::now().timestamp() as f64 + 3600.0));
        let new = jwt("user_new", Some(Utc::now().timestamp() as f64 + 3600.0));
        home.write_token(&old);
        let credential_path = home.credential_path();
        let (listener, endpoint) = listen();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (headers, mut stream) = request_headers(stream).await;
            stream
                .write_all(&response(401, "Retry-After: 60\r\n", b"ignored"))
                .await
                .unwrap();
            fs::write(credential_path, auth_json(&new)).unwrap();
            headers
        });

        let outcome = test_probe(&home, endpoint).probe_at(now()).await;
        let headers = server.await.unwrap();
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::LoginExpired);
        assert!(outcome.retry_after.unwrap() > Utc::now());
        assert!(header_value(&headers, "cookie").is_some());
    }

    #[tokio::test]
    async fn retry_after_http_date_is_preserved_on_parse_failure() {
        let home = TestHome::new();
        home.write_token(&jwt(
            "user_date",
            Some(Utc::now().timestamp() as f64 + 3600.0),
        ));
        let (listener, endpoint) = listen();
        let retry = httpdate::fmt_http_date((Utc::now() + ChronoDuration::seconds(90)).into());
        let server = tokio::spawn(respond(
            listener,
            response(200, &format!("Retry-After: {retry}\r\n"), b"not-json"),
        ));

        let outcome = test_probe(&home, endpoint).probe_at(now()).await;
        let _ = server.await.unwrap();
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
        let deadline = outcome.retry_after.unwrap();
        assert!(deadline > Utc::now());
        assert!(deadline <= Utc::now() + ChronoDuration::seconds(90));
    }

    #[tokio::test]
    async fn delta_retry_after_survives_body_failure_and_is_anchored_at_headers() {
        let home = TestHome::new();
        home.write_token(&jwt(
            "user_body",
            Some(Utc::now().timestamp() as f64 + 3600.0),
        ));
        let (listener, endpoint) = listen();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_headers, mut stream) = request_headers(stream).await;
            stream
                .write_all(&response_with_length(
                    200,
                    "Retry-After: 30\r\n",
                    100,
                    b"partial",
                ))
                .await
                .unwrap();
        });

        let started = Utc::now();
        let outcome = test_probe(&home, endpoint).probe_at(now()).await;
        let finished = Utc::now();
        server.await.unwrap();
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::NetworkError);
        let retry_after = outcome.retry_after.unwrap();
        assert!(retry_after > started);
        assert!(retry_after <= started + ChronoDuration::seconds(31));
        assert!(retry_after > finished);
    }

    #[tokio::test]
    async fn maps_403_without_claiming_subscription_unavailable_and_refuses_redirects() {
        for (status, expected) in [
            (403, ErrorCode::ProviderError),
            (429, ErrorCode::RateLimited),
        ] {
            let home = TestHome::new();
            home.write_token(&jwt(
                "user_status",
                Some(Utc::now().timestamp() as f64 + 3600.0),
            ));
            let (listener, endpoint) = listen();
            let server = tokio::spawn(respond(
                listener,
                response(status, "", b"ignored private body"),
            ));
            let outcome = test_probe(&home, endpoint).probe_at(now()).await;
            let _ = server.await.unwrap();
            assert_eq!(outcome.result.unwrap_err(), expected);
        }

        let home = TestHome::new();
        home.write_token(&jwt(
            "user_redirect",
            Some(Utc::now().timestamp() as f64 + 3600.0),
        ));
        let (listener, endpoint) = listen();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(respond(
            listener,
            response(
                302,
                &format!("Location: http://{address}/redirected\r\n"),
                b"",
            ),
        ));
        let outcome = test_probe(&home, endpoint).probe_at(now()).await;
        let headers = server.await.unwrap();
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
        assert!(header_value(&headers, "cookie").is_some());
    }

    #[tokio::test]
    async fn rejects_declared_and_streamed_oversized_bodies() {
        let home = TestHome::new();
        home.write_token(&jwt(
            "user_large",
            Some(Utc::now().timestamp() as f64 + 3600.0),
        ));
        let (listener, endpoint) = listen();
        let declared = tokio::spawn(respond(
            listener,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_RESPONSE_BYTES + 1
            )
            .into_bytes(),
        ));
        let outcome = test_probe(&home, endpoint).probe_at(now()).await;
        let _ = declared.await.unwrap();
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);

        let home = TestHome::new();
        home.write_token(&jwt(
            "user_chunked",
            Some(Utc::now().timestamp() as f64 + 3600.0),
        ));
        let (listener, endpoint) = listen();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_headers, mut stream) = request_headers(stream).await;
            let mut body = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n", MAX_RESPONSE_BYTES + 1).into_bytes();
            body.extend(std::iter::repeat_n(b'x', MAX_RESPONSE_BYTES + 1));
            body.extend_from_slice(b"\r\n0\r\n\r\n");
            stream.write_all(&body).await.unwrap();
        });
        let outcome = test_probe(&home, endpoint).probe_at(now()).await;
        server.await.unwrap();
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
    }

    #[tokio::test]
    async fn configured_client_has_finite_request_and_body_timeout() {
        let home = TestHome::new();
        home.write_token(&jwt(
            "user_timeout",
            Some(Utc::now().timestamp() as f64 + 3600.0),
        ));
        let (listener, endpoint) = listen();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_headers, _stream) = request_headers(stream).await;
            tokio::time::sleep(Duration::from_millis(400)).await;
        });
        let mut probe = test_probe(&home, endpoint);
        probe.client = crate::providers::http::build_client(
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .unwrap();
        let outcome = probe.probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::NetworkError);
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn finite_production_timeouts_remain_bounded() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(15));
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(5));
    }
}
