use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;

use crate::feed::ErrorCode;
use crate::providers::credential_file::read_json_snapshot;
use crate::providers::http::{build_client, parse_retry_after, read_bounded_body};
#[cfg(not(target_os = "macos"))]
use crate::providers::keychain::SecretBytes;
use crate::providers::keychain::{CredentialReader, KeychainReadError, KeychainTarget};
use crate::quota::{Observation, Quota};
use crate::scheduler::AttemptOutcome;

const USAGE_URL: &str = "https://api.meta.ai/muse-code/key";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const ONBOARD_BODY: &[u8] = br#"{"onboard":false}"#;

/// A file- or Keychain-backed credential value. It has no formatting or
/// serialization implementation and clears its bytes when dropped.
struct Secret(Vec<u8>);

impl Secret {
    fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct MuseCredentials {
    access_token: Secret,
}

/// Read-only Muse quota probe. Its production endpoint is fixed, and the
/// shared client disables proxies, redirects and automatic retries.
pub(crate) struct MuseHttpProbe {
    home: PathBuf,
    xdg_config_home: Option<PathBuf>,
    client: Client,
    usage_url: Url,
    credential_reader: Arc<dyn CredentialReader + Send + Sync>,
}

impl MuseHttpProbe {
    /// Build a convenience client. Construction reads neither credentials nor
    /// provider state and never starts a request.
    pub(crate) fn new(home: PathBuf, xdg_config_home: Option<PathBuf>) -> Result<Self, ErrorCode> {
        let client = build_client(REQUEST_TIMEOUT, CONNECT_TIMEOUT)?;
        Self::with_client(home, xdg_config_home, &client)
    }

    /// Use a clone of the application-owned client while keeping Muse's
    /// endpoint and credential roots fixed by this module.
    pub(crate) fn with_client(
        home: PathBuf,
        xdg_config_home: Option<PathBuf>,
        client: &Client,
    ) -> Result<Self, ErrorCode> {
        Self::with_reader(home, xdg_config_home, client, default_credential_reader())
    }

    fn with_reader(
        home: PathBuf,
        xdg_config_home: Option<PathBuf>,
        client: &Client,
        credential_reader: Arc<dyn CredentialReader + Send + Sync>,
    ) -> Result<Self, ErrorCode> {
        let usage_url = Url::parse(USAGE_URL).map_err(|_| ErrorCode::ProviderError)?;
        Ok(Self {
            home,
            xdg_config_home,
            client: client.clone(),
            usage_url,
            credential_reader,
        })
    }

    /// Make one attempt and return the scheduler's existing result type. The
    /// scheduler owns cooldowns, capture time, caching and persistence.
    pub(crate) async fn probe(&self) -> AttemptOutcome {
        let credentials = match load_credentials(
            &self.home,
            self.xdg_config_home.as_deref(),
            self.credential_reader.as_ref(),
        ) {
            Ok(credentials) => credentials,
            Err(error) => return AttemptOutcome::failure(error),
        };
        let authorization = match authorization_header(&credentials.access_token.0) {
            Some(header) => header,
            None => return AttemptOutcome::failure(ErrorCode::CredentialsMissing),
        };

        let response = match self
            .client
            .post(self.usage_url.clone())
            .header(AUTHORIZATION, authorization)
            .header("x-api-version", HeaderValue::from_static("1.0.0"))
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .body(ONBOARD_BODY)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return AttemptOutcome::failure(ErrorCode::NetworkError),
        };

        // Retry-After starts when response headers arrive, on every status. Save
        // it before status handling or any potentially failing body read.
        let received_at = Utc::now();
        let retry_after = parse_retry_after(response.headers().get(RETRY_AFTER), received_at);
        let status = response.status();
        if status != StatusCode::OK {
            return AttemptOutcome {
                result: Err(status_error(status)),
                retry_after,
            };
        }

        let body = match read_bounded_body(response, MAX_RESPONSE_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                return AttemptOutcome {
                    result: Err(error),
                    retry_after,
                };
            }
        };
        match parse_usage_response(&body, Utc::now()) {
            Ok(observation) => AttemptOutcome {
                result: Ok(observation),
                retry_after,
            },
            Err(error) => AttemptOutcome {
                result: Err(error),
                retry_after,
            },
        }
    }

    #[cfg(test)]
    fn for_loopback_test(
        home: PathBuf,
        xdg_config_home: Option<PathBuf>,
        usage_url: Url,
        timeout: Duration,
        credential_reader: Arc<dyn CredentialReader + Send + Sync>,
    ) -> Self {
        assert_eq!(usage_url.scheme(), "http");
        assert_eq!(usage_url.host_str(), Some("127.0.0.1"));
        assert!(usage_url.port().is_some());
        Self {
            home,
            xdg_config_home,
            client: build_client(timeout, timeout).expect("test HTTP client builds"),
            usage_url,
            credential_reader,
        }
    }
}

#[cfg(target_os = "macos")]
fn default_credential_reader() -> Arc<dyn CredentialReader + Send + Sync> {
    Arc::new(crate::providers::keychain::NativeKeychainReader)
}

#[cfg(not(target_os = "macos"))]
fn default_credential_reader() -> Arc<dyn CredentialReader + Send + Sync> {
    Arc::new(UnsupportedKeychainReader)
}

#[cfg(not(target_os = "macos"))]
struct UnsupportedKeychainReader;

#[cfg(not(target_os = "macos"))]
impl CredentialReader for UnsupportedKeychainReader {
    fn read(&self, _target: KeychainTarget) -> Result<SecretBytes, KeychainReadError> {
        Err(KeychainReadError::InteractionUnavailable)
    }
}

fn status_error(status: StatusCode) -> ErrorCode {
    match status {
        StatusCode::UNAUTHORIZED => ErrorCode::LoginExpired,
        StatusCode::FORBIDDEN => ErrorCode::ProviderError,
        StatusCode::TOO_MANY_REQUESTS => ErrorCode::RateLimited,
        _ => ErrorCode::ProviderError,
    }
}

fn credential_path(home: &Path, xdg_config_home: Option<&Path>) -> Option<PathBuf> {
    if !home.is_absolute() {
        return None;
    }
    let config_home = xdg_config_home
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"));
    Some(config_home.join("muse/auth.json"))
}

fn load_credentials(
    home: &Path,
    xdg_config_home: Option<&Path>,
    reader: &dyn CredentialReader,
) -> Result<MuseCredentials, ErrorCode> {
    let path = credential_path(home, xdg_config_home).ok_or(ErrorCode::CredentialsMissing)?;
    let snapshot = read_json_snapshot(&path, MAX_CREDENTIAL_BYTES)
        .map_err(|_| ErrorCode::CredentialsMissing)?;
    let auth = serde_json::from_slice::<AuthFile>(snapshot.bytes())
        .map_err(|_| ErrorCode::CredentialsMissing)?;
    let meta = auth
        .providers
        .and_then(|providers| providers.meta)
        .filter(|meta| meta.mechanism.as_deref() == Some("oauth"))
        .ok_or(ErrorCode::CredentialsMissing)?;

    if meta.storage.as_deref() == Some("keychain") {
        let item = reader.read(KeychainTarget::Muse).map_err(keychain_error)?;
        if item.expose().len() > MAX_CREDENTIAL_BYTES as usize {
            return Err(ErrorCode::KeychainBlocked);
        }
        let parsed = serde_json::from_slice::<CredentialRecord>(item.expose())
            .map_err(|_| ErrorCode::CredentialsMissing)?;
        return credentials_from_record(parsed);
    }
    credentials_from_record(CredentialRecord {
        access_token: meta.access_token,
        api_key: meta.api_key,
    })
}

fn keychain_error(error: KeychainReadError) -> ErrorCode {
    match error {
        KeychainReadError::ItemNotFound => ErrorCode::CredentialsMissing,
        KeychainReadError::InteractionUnavailable
        | KeychainReadError::CredentialTooLarge
        | KeychainReadError::ReadFailed => ErrorCode::KeychainBlocked,
    }
}

fn credentials_from_record(mut record: CredentialRecord) -> Result<MuseCredentials, ErrorCode> {
    let mut access_token = record
        .access_token
        .take()
        .filter(|token| token.0.starts_with(b"dca:") && token.0.len() > 4)
        .ok_or(ErrorCode::CredentialsMissing)?;
    let standing_key = record
        .api_key
        .take()
        .filter(|key| std::str::from_utf8(&key.0).is_ok_and(|value| !value.trim().is_empty()))
        .ok_or(ErrorCode::CredentialsMissing)?;
    drop(standing_key);
    Ok(MuseCredentials {
        access_token: Secret::new(std::mem::take(&mut access_token.0)),
    })
}

fn authorization_header(token: &[u8]) -> Option<HeaderValue> {
    let mut bytes = b"Bearer ".to_vec();
    bytes.extend_from_slice(token);
    let parsed = HeaderValue::from_bytes(&bytes);
    bytes.fill(0);
    let mut value = parsed.ok()?;
    value.set_sensitive(true);
    Some(value)
}

fn parse_usage_response(
    input: &[u8],
    captured_at: DateTime<Utc>,
) -> Result<Observation, ErrorCode> {
    if input.len() > MAX_RESPONSE_BYTES {
        return Err(ErrorCode::ProviderError);
    }
    let response: MuseResponse =
        serde_json::from_slice(input).map_err(|_| ErrorCode::ProviderError)?;
    if !response.is_subs_active {
        return Err(ErrorCode::SubscriptionUnavailable);
    }

    let quotas = response
        .subs_usage
        .into_iter()
        .flat_map(|usage| {
            [
                quota(usage.window, "session", "Session", 5 * 60 * 60, captured_at),
                quota(
                    usage.weekly,
                    "weekly",
                    "Weekly",
                    7 * 24 * 60 * 60,
                    captured_at,
                ),
            ]
        })
        .flatten()
        .collect();

    Ok(Observation {
        captured_at,
        tier: response.subs_tier_name,
        quotas,
    })
}

fn quota(
    window: Option<MuseWindow>,
    key: &str,
    label: &str,
    window_duration_seconds: i64,
    captured_at: DateTime<Utc>,
) -> Option<Quota> {
    let window = window?;
    let used_percent = window.used_percent.filter(|value| value.is_finite())?;
    let resets_at = match window.resets_at {
        Some(timestamp) => {
            let reset = epoch_seconds(timestamp)?;
            if reset <= captured_at {
                return None;
            }
            Some(reset)
        }
        None => None,
    };
    Some(Quota {
        key: key.to_owned(),
        label: label.to_owned(),
        percent_remaining: Some(100.0 - used_percent.clamp(0.0, 100.0)),
        balance_remaining: None,
        balance_used: None,
        balance_cap: None,
        balance_unit: None,
        units_used: None,
        units_limit: None,
        resets_at,
        window_duration_seconds: Some(window_duration_seconds),
    })
}

fn epoch_seconds(timestamp: f64) -> Option<DateTime<Utc>> {
    if !timestamp.is_finite() {
        return None;
    }
    let seconds = timestamp.floor();
    if seconds < i64::MIN as f64 || seconds >= i64::MAX as f64 {
        return None;
    }
    let mut seconds = seconds as i64;
    let mut nanos = ((timestamp - seconds as f64) * 1_000_000_000.0).round() as u32;
    if nanos == 1_000_000_000 {
        seconds = seconds.checked_add(1)?;
        nanos = 0;
    }
    DateTime::from_timestamp(seconds, nanos)
}

#[derive(Deserialize)]
struct AuthFile {
    providers: Option<Providers>,
}

#[derive(Deserialize)]
struct Providers {
    meta: Option<Meta>,
}

#[derive(Deserialize)]
struct Meta {
    mechanism: Option<String>,
    storage: Option<String>,
    access_token: Option<SecretText>,
    api_key: Option<SecretText>,
}

#[derive(Deserialize)]
struct CredentialRecord {
    access_token: Option<SecretText>,
    api_key: Option<SecretText>,
}

/// Deserialize secret strings directly into an opaque byte owner so each
/// parsed copy is cleared on drop and cannot be formatted accidentally.
struct SecretText(Vec<u8>);

impl<'de> Deserialize<'de> for SecretText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(|value| Self(value.into_bytes()))
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Deserialize)]
struct MuseResponse {
    is_subs_active: bool,
    subs_tier_name: Option<String>,
    subs_usage: Option<MuseUsage>,
}

#[derive(Deserialize)]
struct MuseUsage {
    window: Option<MuseWindow>,
    weekly: Option<MuseWindow>,
}

#[derive(Deserialize)]
struct MuseWindow {
    used_percent: Option<f64>,
    resets_at: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CREDENTIAL_BYTES, MAX_RESPONSE_BYTES, MuseHttpProbe, credential_path, epoch_seconds,
        load_credentials, parse_usage_response,
    };
    use crate::feed::ErrorCode;
    use crate::providers::keychain::{
        CredentialReader, KeychainReadError, KeychainTarget, SecretBytes,
    };
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    use reqwest::Url;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const BODY: &str = r#"{"is_subs_active":true,"subs_tier_name":"Muse Pro","api_key":"returned-secret","user_email":"private@example.invalid","subs_usage":{"window":{"used_percent":25,"window_duration_mins":300,"resets_at":2000000300.25},"weekly":{"used_percent":40,"resets_at":2000600000}}}"#;

    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Self {
            let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("quotabard-muse-{}-{sequence}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write_auth(&self, xdg: Option<&Path>, bytes: &[u8]) -> PathBuf {
            let path = credential_path(self.path(), xdg).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            path
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct FakeReader {
        result: Mutex<Result<Vec<u8>, KeychainReadError>>,
        calls: AtomicUsize,
        targets: Mutex<Vec<KeychainTarget>>,
    }

    impl FakeReader {
        fn with_data(bytes: &[u8]) -> Self {
            Self::new(Ok(bytes.to_vec()))
        }

        fn with_error(error: KeychainReadError) -> Self {
            Self::new(Err(error))
        }

        fn new(result: Result<Vec<u8>, KeychainReadError>) -> Self {
            Self {
                result: Mutex::new(result),
                calls: AtomicUsize::new(0),
                targets: Mutex::new(Vec::new()),
            }
        }
    }

    impl CredentialReader for FakeReader {
        fn read(&self, target: KeychainTarget) -> Result<SecretBytes, KeychainReadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.targets.lock().unwrap().push(target);
            self.result
                .lock()
                .unwrap()
                .as_ref()
                .map(|bytes| SecretBytes::from_bytes(bytes.clone()))
                .map_err(|error| *error)
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(2_000_000_000, 0).unwrap()
    }

    fn file_credentials(token: &str, key: &str) -> Vec<u8> {
        format!(
            r#"{{"providers":{{"meta":{{"mechanism":"oauth","storage":"file","access_token":"{token}","api_key":"{key}"}}}}}}"#
        )
        .into_bytes()
    }

    fn reader() -> Arc<FakeReader> {
        Arc::new(FakeReader::with_error(KeychainReadError::ItemNotFound))
    }

    fn listen() -> (TcpListener, Url) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = TcpListener::from_std(listener).unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = Url::parse(&format!("http://{address}/muse-code/key")).unwrap();
        (listener, endpoint)
    }

    async fn receive_request(
        stream: TcpStream,
    ) -> (String, Vec<(String, String)>, Vec<u8>, TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request_line = line.trim_end().to_owned();
        let mut headers = Vec::new();
        let mut content_length = 0_usize;
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).await.unwrap();
        (request_line, headers, body, reader.into_inner())
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    async fn serve_once(
        listener: TcpListener,
        response: Vec<u8>,
    ) -> (String, Vec<(String, String)>, Vec<u8>) {
        let (stream, _) = listener.accept().await.unwrap();
        let (request_line, headers, body, mut stream) = receive_request(stream).await;
        stream.write_all(&response).await.unwrap();
        (request_line, headers, body)
    }

    async fn serve_and_check_no_second_request(
        listener: TcpListener,
        response: Vec<u8>,
    ) -> (String, bool) {
        let (stream, _) = listener.accept().await.unwrap();
        let (request_line, _, _, mut stream) = receive_request(stream).await;
        stream.write_all(&response).await.unwrap();
        let second_request = tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_ok();
        (request_line, second_request)
    }

    fn http_response(status: u16, extra_headers: &str, body: &[u8]) -> Vec<u8> {
        let content_length = if extra_headers.lines().any(|line| {
            line.split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        }) {
            String::new()
        } else {
            format!("Content-Length: {}\r\n", body.len())
        };
        let mut response = format!(
            "HTTP/1.1 {status} synthetic\r\n{extra_headers}{content_length}Connection: close\r\n\r\n"
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn probe_for(home: &TestHome, endpoint: Url, reader: Arc<FakeReader>) -> MuseHttpProbe {
        MuseHttpProbe::for_loopback_test(
            home.path().to_path_buf(),
            None,
            endpoint,
            Duration::from_secs(2),
            reader,
        )
    }

    #[test]
    fn absolute_xdg_wins_and_relative_xdg_is_ignored() {
        let home = Path::new("/synthetic-home");
        assert_eq!(
            credential_path(home, Some(Path::new("relative-config"))),
            Some(home.join(".config/muse/auth.json"))
        );
        assert_eq!(
            credential_path(home, Some(Path::new("/synthetic-config"))),
            Some(Path::new("/synthetic-config/muse/auth.json").to_path_buf())
        );
        assert_eq!(credential_path(Path::new("relative-home"), None), None);
    }

    #[test]
    fn file_oauth_requires_standing_key_and_never_changes_cli_file() {
        let home = TestHome::new();
        let path = home.write_auth(
            None,
            &file_credentials("dca:synthetic-oauth", "synthetic-key"),
        );
        let original = fs::read(&path).unwrap();
        let mode_before = fs::metadata(&path).unwrap().permissions();
        let credentials = load_credentials(home.path(), None, &*reader()).unwrap();
        assert!(
            credentials
                .access_token
                .0
                .starts_with(b"dca:synthetic-oauth")
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::metadata(&path).unwrap().permissions(), mode_before);

        let missing_key = file_credentials("dca:synthetic-oauth", "   ");
        home.write_auth(None, &missing_key);
        assert_eq!(
            load_credentials(home.path(), None, &*reader())
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
        home.write_auth(None, br#"{"providers":{"meta":{"mechanism":"api_key","access_token":"inference-only","api_key":"key"}}}"#);
        assert_eq!(
            load_credentials(home.path(), None, &*reader())
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
    }

    #[test]
    fn file_loader_uses_absolute_xdg_and_ignores_relative_xdg() {
        let home = TestHome::new();
        let custom = home.path().join("custom-config");
        home.write_auth(
            Some(&custom),
            &file_credentials("dca:xdg-token", "standing-key"),
        );
        home.write_auth(None, &file_credentials("dca:home-token", "standing-key"));

        let custom_credentials = load_credentials(home.path(), Some(&custom), &*reader()).unwrap();
        assert_eq!(custom_credentials.access_token.0, b"dca:xdg-token");
        let relative = Path::new("relative-config");
        let fallback_credentials =
            load_credentials(home.path(), Some(relative), &*reader()).unwrap();
        assert_eq!(fallback_credentials.access_token.0, b"dca:home-token");
    }

    #[test]
    fn keychain_descriptor_selects_only_fixed_muse_target_and_requires_both_keys() {
        let home = TestHome::new();
        let descriptor = br#"{"providers":{"meta":{"storage":"keychain","mechanism":"oauth","access_token":"dca:file-token","api_key":"file-key","service":"attacker-service","account":"attacker-account"}}}"#;
        home.write_auth(None, descriptor);
        let fake = Arc::new(FakeReader::with_data(
            br#"{"access_token":"dca:keychain-token","api_key":"standing-key"}"#,
        ));
        let credentials = load_credentials(home.path(), None, fake.as_ref()).unwrap();
        assert_eq!(credentials.access_token.0, b"dca:keychain-token");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert!(*fake.targets.lock().unwrap() == [KeychainTarget::Muse]);

        let no_standing_key = Arc::new(FakeReader::with_data(br#"{"access_token":"dca:oauth"}"#));
        assert_eq!(
            load_credentials(home.path(), None, no_standing_key.as_ref())
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
        let no_oauth = Arc::new(FakeReader::with_data(br#"{"api_key":"standing"}"#));
        assert_eq!(
            load_credentials(home.path(), None, no_oauth.as_ref())
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
    }

    #[test]
    fn keychain_failures_are_safe_and_missing_item_is_not_reported_as_blocked() {
        let home = TestHome::new();
        home.write_auth(
            None,
            br#"{"providers":{"meta":{"storage":"keychain","mechanism":"oauth"}}}"#,
        );
        let blocked = Arc::new(FakeReader::with_error(
            KeychainReadError::InteractionUnavailable,
        ));
        assert_eq!(
            load_credentials(home.path(), None, blocked.as_ref())
                .err()
                .unwrap(),
            ErrorCode::KeychainBlocked
        );
        let absent = Arc::new(FakeReader::with_error(KeychainReadError::ItemNotFound));
        assert_eq!(
            load_credentials(home.path(), None, absent.as_ref())
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
    }

    #[test]
    fn credential_file_bound_rejects_more_than_one_mib() {
        let home = TestHome::new();
        let path = home.write_auth(None, &vec![b'x'; MAX_CREDENTIAL_BYTES as usize + 1]);
        assert_eq!(
            load_credentials(home.path(), None, &*reader())
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
        assert!(fs::metadata(path).unwrap().is_file());
    }

    #[test]
    fn parses_active_subscription_tier_windows_and_fractional_resets() {
        let observation = parse_usage_response(BODY.as_bytes(), now()).unwrap();
        assert_eq!(observation.tier.as_deref(), Some("Muse Pro"));
        assert_eq!(observation.quotas.len(), 2);
        assert_eq!(observation.quotas[0].key, "session");
        assert_eq!(observation.quotas[0].percent_remaining, Some(75.0));
        assert_eq!(observation.quotas[0].window_duration_seconds, Some(18_000));
        assert_eq!(
            observation.quotas[0].resets_at,
            Some(DateTime::from_timestamp(2_000_000_300, 250_000_000).unwrap())
        );
        assert_eq!(observation.quotas[1].key, "weekly");
        assert_eq!(observation.quotas[1].percent_remaining, Some(60.0));
        assert_eq!(observation.quotas[1].window_duration_seconds, Some(604_800));
        let json = serde_json::to_vec(&observation.quotas[0].key).unwrap();
        assert!(!String::from_utf8(json).unwrap().contains("returned-secret"));
    }

    #[test]
    fn explicit_inactive_subscription_is_distinct_but_forbidden_is_not() {
        assert_eq!(
            parse_usage_response(br#"{"is_subs_active":false}"#, now()).unwrap_err(),
            ErrorCode::SubscriptionUnavailable
        );
        assert_eq!(
            super::status_error(reqwest::StatusCode::FORBIDDEN),
            ErrorCode::ProviderError
        );
        assert_eq!(
            super::status_error(reqwest::StatusCode::UNAUTHORIZED),
            ErrorCode::LoginExpired
        );
    }

    #[test]
    fn absent_expired_and_invalid_windows_are_omitted_not_filled() {
        let idle = parse_usage_response(br#"{"is_subs_active":true}"#, now()).unwrap();
        assert!(idle.quotas.is_empty());
        let partial = parse_usage_response(
            br#"{"is_subs_active":true,"subs_usage":{"window":{"used_percent":0,"resets_at":1999999999},"weekly":{"used_percent":20,"resets_at":2000000001}}}"#,
            now(),
        )
        .unwrap();
        assert_eq!(partial.quotas.len(), 1);
        assert_eq!(partial.quotas[0].key, "weekly");
        assert_eq!(partial.quotas[0].percent_remaining, Some(80.0));
        let without_reset = parse_usage_response(
            br#"{"is_subs_active":true,"subs_usage":{"window":{"used_percent":80}}}"#,
            now(),
        )
        .unwrap();
        assert_eq!(without_reset.quotas.len(), 1);
        assert_eq!(without_reset.quotas[0].percent_remaining, Some(20.0));
        assert_eq!(without_reset.quotas[0].resets_at, None);
        assert_eq!(
            parse_usage_response(br#"{"is_subs_active":true,"subs_usage":{"window":{"used_percent":10,"resets_at":1e100}}}"#, now()).unwrap().quotas.len(),
            0
        );
    }

    #[test]
    fn used_percent_is_clamped_and_malformed_json_is_safe() {
        let observation = parse_usage_response(
            br#"{"is_subs_active":true,"subs_usage":{"window":{"used_percent":140},"weekly":{"used_percent":-20}}}"#,
            now(),
        )
        .unwrap();
        assert_eq!(observation.quotas[0].percent_remaining, Some(0.0));
        assert_eq!(observation.quotas[1].percent_remaining, Some(100.0));
        assert_eq!(
            parse_usage_response(b"not-json", now()),
            Err(ErrorCode::ProviderError)
        );
    }

    #[test]
    fn epoch_conversion_rejects_nonfinite_and_preserves_fraction() {
        assert_eq!(epoch_seconds(f64::NAN), None);
        assert_eq!(epoch_seconds(f64::INFINITY), None);
        assert_eq!(
            epoch_seconds(2_000_000_000.125),
            Some(DateTime::from_timestamp(2_000_000_000, 125_000_000).unwrap())
        );
    }

    #[tokio::test]
    async fn sends_fixed_post_endpoint_auth_version_headers_and_onboard_false() {
        let home = TestHome::new();
        home.write_auth(
            None,
            &file_credentials("dca:synthetic-muse-token", "standing-key"),
        );
        let (listener, endpoint) = listen();
        let response = http_response(200, "Content-Type: application/json\r\n", BODY.as_bytes());
        let server = tokio::spawn(serve_once(listener, response));
        let outcome = probe_for(&home, endpoint, reader()).probe().await;
        let (request, headers, body) = server.await.unwrap();
        assert_eq!(request, "POST /muse-code/key HTTP/1.1");
        assert_eq!(
            header(&headers, "authorization"),
            Some("Bearer dca:synthetic-muse-token")
        );
        assert_eq!(header(&headers, "x-api-version"), Some("1.0.0"));
        assert_eq!(header(&headers, "content-type"), Some("application/json"));
        assert_eq!(header(&headers, "accept"), Some("application/json"));
        assert_eq!(body, br#"{"onboard":false}"#);
        assert!(
            !headers
                .iter()
                .any(|(_, value)| value.contains("standing-key"))
        );
        assert!(!String::from_utf8_lossy(&body).contains("standing-key"));
        assert_eq!(outcome.result.unwrap().quotas.len(), 2);
    }

    #[tokio::test]
    async fn maps_401_403_429_and_does_not_retry_any_status() {
        for (status, expected) in [
            (401, ErrorCode::LoginExpired),
            (403, ErrorCode::ProviderError),
            (429, ErrorCode::RateLimited),
            (500, ErrorCode::ProviderError),
        ] {
            let home = TestHome::new();
            home.write_auth(
                None,
                &file_credentials("dca:synthetic-token", "standing-key"),
            );
            let (listener, endpoint) = listen();
            let response = http_response(status, "", b"synthetic private error body");
            let server = tokio::spawn(serve_and_check_no_second_request(listener, response));
            let outcome = probe_for(&home, endpoint, reader()).probe().await;
            let (request, second_request) = server.await.unwrap();
            assert_eq!(request, "POST /muse-code/key HTTP/1.1");
            assert!(!second_request);
            assert_eq!(outcome.result.unwrap_err(), expected);
            assert!(outcome.retry_after.is_none());
        }
    }

    #[tokio::test]
    async fn declared_and_chunked_oversized_bodies_are_rejected() {
        let home = TestHome::new();
        home.write_auth(
            None,
            &file_credentials("dca:synthetic-token", "standing-key"),
        );
        let (listener, endpoint) = listen();
        let declared = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let server = tokio::spawn(serve_once(listener, declared));
        assert_eq!(
            probe_for(&home, endpoint, reader())
                .probe()
                .await
                .result
                .unwrap_err(),
            ErrorCode::ProviderError
        );
        server.await.unwrap();

        let home = TestHome::new();
        home.write_auth(
            None,
            &file_credentials("dca:synthetic-token", "standing-key"),
        );
        let (listener, endpoint) = listen();
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        chunked.extend(std::iter::repeat_n(b'x', MAX_RESPONSE_BYTES + 1));
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let server = tokio::spawn(serve_once(listener, chunked));
        assert_eq!(
            probe_for(&home, endpoint, reader())
                .probe()
                .await
                .result
                .unwrap_err(),
            ErrorCode::ProviderError
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refuses_redirect_without_sending_a_second_request() {
        let home = TestHome::new();
        home.write_auth(
            None,
            &file_credentials("dca:synthetic-token", "standing-key"),
        );
        let (listener, endpoint) = listen();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, _, _, mut stream) = receive_request(stream).await;
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{address}/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_ok()
        });
        let outcome = probe_for(&home, endpoint, reader()).probe().await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
        assert!(!server.await.unwrap());
    }

    #[tokio::test]
    async fn retry_after_is_kept_from_headers_when_body_is_oversized() {
        let home = TestHome::new();
        home.write_auth(
            None,
            &file_credentials("dca:synthetic-token", "standing-key"),
        );
        let (listener, endpoint) = listen();
        let retry = httpdate::fmt_http_date((Utc::now() + ChronoDuration::seconds(120)).into());
        let expected = DateTime::<Utc>::from(httpdate::parse_http_date(&retry).unwrap());
        let headers = format!(
            "Retry-After: {retry}\r\nContent-Length: {}\r\n",
            MAX_RESPONSE_BYTES + 1
        );
        let server = tokio::spawn(serve_once(listener, http_response(200, &headers, b"")));
        let outcome = probe_for(&home, endpoint, reader()).probe().await;
        server.await.unwrap();
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
        assert_eq!(outcome.retry_after, Some(expected));
    }

    #[tokio::test]
    async fn retry_after_survives_a_truncated_body_read_failure() {
        let home = TestHome::new();
        home.write_auth(
            None,
            &file_credentials("dca:synthetic-token", "standing-key"),
        );
        let (listener, endpoint) = listen();
        let retry = httpdate::fmt_http_date((Utc::now() + ChronoDuration::seconds(120)).into());
        let expected = DateTime::<Utc>::from(httpdate::parse_http_date(&retry).unwrap());
        let response = format!(
            "HTTP/1.1 200 OK\r\nRetry-After: {retry}\r\nContent-Length: 80\r\nConnection: close\r\n\r\npartial"
        )
        .into_bytes();
        let server = tokio::spawn(serve_once(listener, response));
        let outcome = probe_for(&home, endpoint, reader()).probe().await;
        server.await.unwrap();
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::NetworkError);
        assert_eq!(outcome.retry_after, Some(expected));
    }

    #[tokio::test]
    async fn future_retry_after_is_retained_for_auth_forbidden_rate_limit_and_parse_failure() {
        for status in [200, 401, 403, 429] {
            let home = TestHome::new();
            home.write_auth(
                None,
                &file_credentials("dca:synthetic-token", "standing-key"),
            );
            let (listener, endpoint) = listen();
            let retry = "Retry-After: 90\r\n";
            let body = if status == 200 {
                b"not-json".as_slice()
            } else {
                b"ignored".as_slice()
            };
            let server = tokio::spawn(serve_once(listener, http_response(status, retry, body)));
            let before = Utc::now();
            let outcome = probe_for(&home, endpoint, reader()).probe().await;
            let after = Utc::now();
            server.await.unwrap();
            assert!(outcome.retry_after.unwrap() > before);
            assert!(outcome.retry_after.unwrap() <= after + ChronoDuration::seconds(90));
            if status == 200 {
                assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
            }
        }
    }

    #[tokio::test]
    async fn client_times_out_finitely_when_loopback_stalls() {
        assert_eq!(super::REQUEST_TIMEOUT, Duration::from_secs(15));
        assert_eq!(super::CONNECT_TIMEOUT, Duration::from_secs(5));
        let home = TestHome::new();
        home.write_auth(
            None,
            &file_credentials("dca:synthetic-token", "standing-key"),
        );
        let (listener, endpoint) = listen();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = receive_request(stream).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        });
        let started = std::time::Instant::now();
        let outcome = MuseHttpProbe::for_loopback_test(
            home.path().to_path_buf(),
            None,
            endpoint,
            Duration::from_millis(80),
            reader(),
        )
        .probe()
        .await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::NetworkError);
        assert!(started.elapsed() < Duration::from_secs(1));
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn authorization_header_is_marked_sensitive_and_rejects_invalid_token_bytes() {
        let header = super::authorization_header(b"dca:synthetic-token").unwrap();
        assert!(header.is_sensitive());
        assert!(super::authorization_header(b"dca:bad\r\ntoken").is_none());
    }

    #[test]
    fn response_parser_enforces_the_same_size_limit_as_http_reader() {
        assert_eq!(
            parse_usage_response(&vec![b' '; MAX_RESPONSE_BYTES + 1], now()).unwrap_err(),
            ErrorCode::ProviderError
        );
    }
}
