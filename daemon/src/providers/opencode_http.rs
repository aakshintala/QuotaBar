use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use reqwest::{Client, Url};

use crate::providers::http::{build_client, parse_retry_after, read_bounded_body};
use serde::Deserialize;

use crate::feed::ErrorCode;
use crate::providers::opencode::parse_usage;
use crate::quota::Observation;

const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// OpenCode Go's API key. Deliberately has no Debug or serialization support.
struct Secret(String);

/// Safe provider result plus metadata needed by the scheduler and opt-in check.
/// It deliberately does not implement Debug, so observations and headers cannot
/// be accidentally dumped while handling a failure.
pub struct OpenCodeHttpOutcome {
    pub result: Result<Observation, ErrorCode>,
    pub retry_after: Option<DateTime<Utc>>,
    pub http_status: Option<u16>,
}

/// Read-only OpenCode Go usage probe. The production constructor always targets
/// the provider's fixed HTTPS endpoint and disables proxies and redirects.
pub struct OpenCodeHttpProbe {
    home: PathBuf,
    xdg_data_home: Option<PathBuf>,
    client: Client,
    usage_url: Url,
}

impl OpenCodeHttpProbe {
    /// Construct a production probe rooted at explicit service environment paths.
    pub fn new(home: PathBuf, xdg_data_home: Option<PathBuf>) -> Result<Self, ErrorCode> {
        let client = build_client(REQUEST_TIMEOUT, CONNECT_TIMEOUT)?;
        Self::with_client(home, xdg_data_home, client)
    }

    /// Construct with a caller-configured shared client while retaining the
    /// fixed provider endpoint and explicit credential roots.
    pub(crate) fn with_client(
        home: PathBuf,
        xdg_data_home: Option<PathBuf>,
        client: Client,
    ) -> Result<Self, ErrorCode> {
        let usage_url = Url::parse(USAGE_URL).map_err(|_| ErrorCode::ProviderError)?;
        Ok(Self {
            home,
            xdg_data_home,
            client,
            usage_url,
        })
    }

    /// Perform exactly one bounded request. `now` is the clock sample used for
    /// parsed quota timestamps; Retry-After is anchored to wall-clock time when
    /// response headers arrive. The scheduler replaces capture time when the
    /// attempt completes.
    pub async fn probe_at(&self, now: DateTime<Utc>) -> OpenCodeHttpOutcome {
        let secret = match load_api_key(&self.home, self.xdg_data_home.as_deref()) {
            Ok(secret) => secret,
            Err(error) => return failure(error, None, None),
        };
        let authorization = match authorization_header(&secret) {
            Some(header) => header,
            None => return failure(ErrorCode::CredentialsMissing, None, None),
        };

        let response = match self
            .client
            .get(self.usage_url.clone())
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .header(AUTHORIZATION, authorization)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return failure(ErrorCode::NetworkError, None, None),
        };

        // `send` resolves when response headers arrive. Sample before status
        // handling or body reads so delta-seconds begin at the server's reply.
        let headers_received_at = Utc::now();
        let status = response.status();
        let http_status = Some(status.as_u16());
        let retry_after =
            parse_retry_after(response.headers().get("retry-after"), headers_received_at);
        let error = match status.as_u16() {
            401 => Some(ErrorCode::LoginExpired),
            403 => Some(ErrorCode::ProviderError),
            429 => Some(ErrorCode::RateLimited),
            200..=299 => None,
            _ => Some(ErrorCode::ProviderError),
        };
        if let Some(error) = error {
            return failure(error, retry_after, http_status);
        }

        let body = match read_bounded_body(response, MAX_RESPONSE_BYTES).await {
            Ok(body) => body,
            Err(error) => return failure(error, retry_after, http_status),
        };
        match parse_usage(&body, now) {
            Ok(observation) => OpenCodeHttpOutcome {
                result: Ok(observation),
                retry_after,
                http_status,
            },
            Err(_) => failure(ErrorCode::ProviderError, retry_after, http_status),
        }
    }

    #[cfg(test)]
    fn for_loopback_test(
        home: PathBuf,
        xdg_data_home: Option<PathBuf>,
        usage_url: Url,
        timeout: Duration,
    ) -> Self {
        assert_eq!(usage_url.scheme(), "http");
        assert_eq!(usage_url.host_str(), Some("127.0.0.1"));
        Self {
            home,
            xdg_data_home,
            client: build_client(timeout, timeout).expect("test HTTP client builds"),
            usage_url,
        }
    }
}

fn failure(
    error: ErrorCode,
    retry_after: Option<DateTime<Utc>>,
    http_status: Option<u16>,
) -> OpenCodeHttpOutcome {
    OpenCodeHttpOutcome {
        result: Err(error),
        retry_after,
        http_status,
    }
}

fn credential_path(home: &Path, xdg_data_home: Option<&Path>) -> PathBuf {
    let data_home = xdg_data_home
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local/share"));
    data_home.join("opencode/auth.json")
}

fn load_api_key(home: &Path, xdg_data_home: Option<&Path>) -> Result<Secret, ErrorCode> {
    let path = credential_path(home, xdg_data_home);
    let file = File::open(path).map_err(|_| ErrorCode::CredentialsMissing)?;
    let mut bytes = Vec::new();
    file.take(MAX_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ErrorCode::CredentialsMissing)?;
    if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(ErrorCode::CredentialsMissing);
    }

    let credentials: CredentialFile =
        serde_json::from_slice(&bytes).map_err(|_| ErrorCode::CredentialsMissing)?;
    let credential = credentials
        .opencode_go
        .filter(|credential| credential.credential_type == "api")
        .ok_or(ErrorCode::CredentialsMissing)?;
    let key = credential.key.filter(|key| {
        !key.is_empty()
            && !key
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    });
    key.map(Secret).ok_or(ErrorCode::CredentialsMissing)
}

#[derive(Deserialize)]
struct CredentialFile {
    #[serde(rename = "opencode-go")]
    opencode_go: Option<Credential>,
}

#[derive(Deserialize)]
struct Credential {
    #[serde(rename = "type")]
    credential_type: String,
    key: Option<String>,
}

fn authorization_header(secret: &Secret) -> Option<HeaderValue> {
    let authorization = format!("Bearer {}", secret.0);
    let mut value = HeaderValue::from_bytes(authorization.as_bytes()).ok()?;
    value.set_sensitive(true);
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CREDENTIAL_BYTES, MAX_RESPONSE_BYTES, OpenCodeHttpProbe, credential_path, load_api_key,
        parse_retry_after,
    };
    use crate::feed::ErrorCode;
    use crate::runtime_state::RuntimeState;
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    use reqwest::Url;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const FIXTURE: &str = include_str!("../../../Tests/Fixtures/OpenCode/three-windows.json");

    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Self {
            let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quotabard-opencode-http-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write_auth(&self, value: &[u8]) {
            let path = self.path().join(".local/share/opencode/auth.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, value).unwrap();
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn probe_for(home: &TestHome, endpoint: Url) -> OpenCodeHttpProbe {
        OpenCodeHttpProbe::for_loopback_test(
            home.path().to_path_buf(),
            None,
            endpoint,
            Duration::from_secs(2),
        )
    }

    fn listen() -> (TcpListener, Url) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = TcpListener::from_std(listener).unwrap();
        let address = listener.local_addr().unwrap();
        let url = Url::parse(&format!("http://{address}/usage")).unwrap();
        (listener, url)
    }

    async fn serve_once(listener: TcpListener, response: Vec<u8>) -> bool {
        let (stream, _) = listener.accept().await.unwrap();
        let authorized = read_request(stream).await;
        let mut stream = authorized.1;
        let _ = stream.write_all(&response).await;
        authorized.0
    }

    async fn read_request(stream: TcpStream) -> (bool, TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let mut saw_get = false;
        let mut accept_json = false;
        let mut bearer_ok = false;
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                break;
            }
            if line.starts_with("GET /usage HTTP/1.1") {
                saw_get = true;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("accept") {
                    accept_json = value.trim().eq_ignore_ascii_case("application/json");
                }
                if name.eq_ignore_ascii_case("authorization") {
                    bearer_ok = value.trim().as_bytes() == b"Bearer synthetic-opencode-test-key";
                }
            }
        }
        (saw_get && accept_json && bearer_ok, reader.into_inner())
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

    fn auth_with(key: &str) -> Vec<u8> {
        format!(r#"{{"opencode-go":{{"type":"api","key":"{key}"}}}}"#).into_bytes()
    }

    #[test]
    fn absolute_xdg_wins_but_relative_xdg_is_ignored() {
        let home = Path::new("/synthetic-home");
        assert_eq!(
            credential_path(home, Some(Path::new("relative-data"))),
            home.join(".local/share/opencode/auth.json")
        );
        assert_eq!(
            credential_path(home, Some(Path::new("/synthetic-data"))),
            Path::new("/synthetic-data/opencode/auth.json")
        );
    }

    #[test]
    fn missing_malformed_and_unusable_credentials_fail_safely() {
        let home = TestHome::new();
        assert!(matches!(
            load_api_key(home.path(), None),
            Err(ErrorCode::CredentialsMissing)
        ));
        for contents in [
            b"not json".as_slice(),
            br#"{"opencode-go":{"type":"oauth","key":"synthetic"}}"#.as_slice(),
            br#"{"opencode-go":{"type":"api","key":""}}"#.as_slice(),
            br#"{"opencode-go":{"type":"api","key":"bad\r\nheader"}}"#.as_slice(),
            br#"{"opencode-go":{"type":"api"}}"#.as_slice(),
        ] {
            home.write_auth(contents);
            assert!(matches!(
                load_api_key(home.path(), None),
                Err(ErrorCode::CredentialsMissing)
            ));
        }
    }

    #[test]
    fn credential_reads_are_bounded_and_xdg_path_is_explicit() {
        let home = TestHome::new();
        home.write_auth(&vec![b' '; MAX_CREDENTIAL_BYTES as usize + 1]);
        assert!(matches!(
            load_api_key(home.path(), None),
            Err(ErrorCode::CredentialsMissing)
        ));

        let xdg = home.path().join("custom-data");
        let path = xdg.join("opencode/auth.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, auth_with("synthetic-xdg-key")).unwrap();
        assert!(load_api_key(home.path(), Some(&xdg)).is_ok());
    }

    #[test]
    fn parses_only_future_delta_and_http_date_retry_after_values() {
        let now = now();
        let delta = reqwest::header::HeaderValue::from_static("45");
        assert_eq!(
            parse_retry_after(Some(&delta), now),
            Some(now + ChronoDuration::seconds(45))
        );
        let past = reqwest::header::HeaderValue::from_static("0");
        assert_eq!(parse_retry_after(Some(&past), now), None);
        let date = httpdate::fmt_http_date((now + ChronoDuration::seconds(90)).into());
        let date = reqwest::header::HeaderValue::from_bytes(date.as_bytes()).unwrap();
        assert_eq!(
            parse_retry_after(Some(&date), now),
            Some(now + ChronoDuration::seconds(90))
        );
        let invalid = reqwest::header::HeaderValue::from_static("tomorrow");
        assert_eq!(parse_retry_after(Some(&invalid), now), None);
    }

    #[tokio::test]
    async fn success_reuses_shared_parser_and_sends_only_safe_expected_headers() {
        let home = TestHome::new();
        home.write_auth(&auth_with("synthetic-opencode-test-key"));
        let (listener, endpoint) = listen();
        let response = response(
            200,
            "Content-Type: application/json\r\n",
            FIXTURE.as_bytes(),
        );
        let server = tokio::spawn(serve_once(listener, response));

        let result = probe_for(&home, endpoint).probe_at(now()).await;
        assert_eq!(result.http_status, Some(200));
        assert_eq!(result.retry_after, None);
        let observation = result.result.unwrap();
        assert_eq!(observation.quotas.len(), 3);
        assert!((observation.quotas[0].percent_remaining.unwrap() - 79.16667).abs() < 0.00001);
        assert!(server.await.unwrap());
    }

    #[tokio::test]
    async fn maps_auth_forbidden_and_rate_limit_without_treating_forbidden_as_expired() {
        for (status, expected) in [
            (401, ErrorCode::LoginExpired),
            (403, ErrorCode::ProviderError),
            (429, ErrorCode::RateLimited),
        ] {
            let home = TestHome::new();
            home.write_auth(&auth_with("synthetic-opencode-test-key"));
            let (listener, endpoint) = listen();
            let retry = if status == 429 {
                "Retry-After: 30\r\n"
            } else {
                ""
            };
            let server = tokio::spawn(serve_once(
                listener,
                response(status, retry, b"ignored error body"),
            ));

            let result = probe_for(&home, endpoint).probe_at(now()).await;
            assert_eq!(result.result.unwrap_err(), expected);
            assert_eq!(result.http_status, Some(status));
            match result.retry_after {
                Some(deadline) if status == 429 => {
                    let observed_at = Utc::now();
                    assert!(deadline > observed_at);
                    assert!(deadline <= observed_at + ChronoDuration::seconds(30));
                }
                None if status != 429 => {}
                _ => panic!("unexpected Retry-After metadata for HTTP status {status}"),
            }
            assert!(server.await.unwrap());
        }
    }

    #[tokio::test]
    async fn reads_http_date_retry_after_from_success_and_errors() {
        for status in [200, 503] {
            let home = TestHome::new();
            home.write_auth(&auth_with("synthetic-opencode-test-key"));
            let (listener, endpoint) = listen();
            let retry = httpdate::fmt_http_date((Utc::now() + ChronoDuration::seconds(120)).into());
            let retry_deadline = DateTime::<Utc>::from(httpdate::parse_http_date(&retry).unwrap());
            let headers = format!("Retry-After: {retry}\r\nContent-Type: application/json\r\n");
            let body = if status == 200 {
                FIXTURE.as_bytes()
            } else {
                b"ignored"
            };
            let server = tokio::spawn(serve_once(listener, response(status, &headers, body)));

            let result = probe_for(&home, endpoint).probe_at(now()).await;
            assert_eq!(result.retry_after, Some(retry_deadline));
            if status == 503 {
                assert_eq!(result.result.unwrap_err(), ErrorCode::ProviderError);
            } else {
                assert!(result.result.is_ok());
            }
            assert!(server.await.unwrap());
        }
    }

    #[tokio::test]
    async fn delayed_retry_after_survives_body_failure_and_persists() {
        let home = TestHome::new();
        home.write_auth(&auth_with("synthetic-opencode-test-key"));
        let (listener, endpoint) = listen();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (authorized, mut stream) = read_request(stream).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nRetry-After: 1\r\nContent-Length: 64\r\nConnection: close\r\n\r\npartial",
                )
                .await;
            authorized
        });
        let probe = OpenCodeHttpProbe::for_loopback_test(
            home.path().to_path_buf(),
            None,
            endpoint,
            Duration::from_secs(5),
        );

        let request_started = Utc::now();
        let outcome = probe.probe_at(request_started).await;
        let finished = Utc::now();
        assert!(finished.signed_duration_since(request_started) >= ChronoDuration::seconds(2));
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::NetworkError);
        assert_eq!(outcome.http_status, Some(200));
        let retry_after = outcome
            .retry_after
            .expect("Retry-After retained after body failure");
        assert!(retry_after > finished);
        assert!(server.await.unwrap());

        {
            let mut state = RuntimeState::open(home.path()).unwrap();
            assert_eq!(
                state
                    .extend_cooldown("opencode-go", retry_after, finished)
                    .unwrap(),
                retry_after
            );
        }
        let mut reopened = RuntimeState::open(home.path()).unwrap();
        assert_eq!(
            reopened
                .load_cooldown("opencode-go", Utc::now())
                .unwrap()
                .not_before,
            Some(retry_after)
        );
    }

    #[tokio::test]
    async fn refuses_redirects_and_never_forwards_the_bearer_header() {
        let home = TestHome::new();
        home.write_auth(&auth_with("synthetic-opencode-test-key"));
        let (listener, endpoint) = listen();
        let address = listener.local_addr().unwrap();
        let response = response(
            302,
            &format!("Location: http://{address}/redirected\r\n"),
            b"",
        );
        let server = tokio::spawn(serve_once(listener, response));

        let result = probe_for(&home, endpoint).probe_at(now()).await;
        assert_eq!(result.result.unwrap_err(), ErrorCode::ProviderError);
        assert_eq!(result.http_status, Some(302));
        assert!(server.await.unwrap());
    }

    #[tokio::test]
    async fn rejects_declared_and_chunked_oversized_response_bodies() {
        let home = TestHome::new();
        home.write_auth(&auth_with("synthetic-opencode-test-key"));
        let (listener, endpoint) = listen();
        let declared = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let server = tokio::spawn(serve_once(listener, declared));
        let result = probe_for(&home, endpoint).probe_at(now()).await;
        assert_eq!(result.result.unwrap_err(), ErrorCode::ProviderError);
        assert!(server.await.unwrap());

        let home = TestHome::new();
        home.write_auth(&auth_with("synthetic-opencode-test-key"));
        let (listener, endpoint) = listen();
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        chunked.extend(std::iter::repeat_n(b'x', MAX_RESPONSE_BYTES + 1));
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let server = tokio::spawn(serve_once(listener, chunked));
        let result = probe_for(&home, endpoint).probe_at(now()).await;
        assert_eq!(result.result.unwrap_err(), ErrorCode::ProviderError);
        assert!(server.await.unwrap());
    }

    #[tokio::test]
    async fn client_has_a_finite_timeout_for_a_stalled_local_response() {
        let home = TestHome::new();
        home.write_auth(&auth_with("synthetic-opencode-test-key"));
        let (listener, endpoint) = listen();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_authorized, _stream) = read_request(stream).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        });
        let probe = OpenCodeHttpProbe::for_loopback_test(
            home.path().to_path_buf(),
            None,
            endpoint,
            Duration::from_millis(100),
        );

        let result = probe.probe_at(now()).await;
        assert_eq!(result.result.unwrap_err(), ErrorCode::NetworkError);
        assert_eq!(result.http_status, None);
        server.abort();
        let _ = server.await;
    }
}
