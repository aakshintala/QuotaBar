use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use reqwest::header::HeaderMap;
use reqwest::{Client, Response, Url};
use serde::{Deserialize, Serialize};

use crate::feed::{FeedDecodeError, decode_feed_v1};
use crate::feed_text::format_feed;
use crate::providers::http::{build_client, read_bounded_body};
use crate::runtime_state::{ControlToken, RuntimeStateError, load_control_token};

const DEFAULT_FEED_URL: &str = "http://127.0.0.1:8787/quotas";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FEED_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 4096;
const MAX_CONTROL_RESPONSE_BYTES: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Invocation {
    Daemon,
    Client(ClientCommand),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClientCommand {
    Quotas { json: bool },
    SetEnabled { id: String, enabled: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CliError {
    Usage,
    InvalidProvider,
    InvalidFeedUrl,
    TokenUnavailable,
    DaemonUnavailable,
    Busy,
    Configuration,
    Unauthorized,
    Persistence,
    InvalidFeed,
    UnsupportedFeedSchema,
    ResponseTooLarge,
    DaemonRejected,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Usage => {
                "usage: quotabard [quota [--json] | quota enable <provider> | quota disable <provider>]"
            }
            Self::InvalidProvider => "unknown provider ID",
            Self::InvalidFeedUrl => "QUOTA_FEED must be an HTTP loopback /quotas URL",
            Self::TokenUnavailable => "daemon control token is unavailable or unsafe",
            Self::DaemonUnavailable => "quota daemon is unavailable",
            Self::Busy => "quota daemon is busy; try again shortly",
            Self::Configuration => "daemon configuration is invalid",
            Self::Unauthorized => "daemon rejected the control request",
            Self::Persistence => "daemon could not save the provider setting",
            Self::InvalidFeed => "quota feed is invalid",
            Self::UnsupportedFeedSchema => "quota feed schema is unsupported",
            Self::ResponseTooLarge => "quota daemon response exceeded its size limit",
            Self::DaemonRejected => "quota daemon rejected the request",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CliError {}

/// Parse client commands before constructing daemon-only state or providers.
pub(crate) fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, CliError> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Invocation::Daemon);
    };
    if first.to_str() != Some("quota") {
        return Err(CliError::Usage);
    }

    let Some(action) = args.next() else {
        return Ok(Invocation::Client(ClientCommand::Quotas { json: false }));
    };
    match action.to_str() {
        Some("--json") if args.next().is_none() => {
            Ok(Invocation::Client(ClientCommand::Quotas { json: true }))
        }
        Some("enable") | Some("disable") => {
            let enabled = action == "enable";
            let id = args
                .next()
                .and_then(|value| value.into_string().ok())
                .ok_or(CliError::Usage)?;
            if args.next().is_some() {
                return Err(CliError::Usage);
            }
            if !valid_provider_id(&id) {
                return Err(CliError::InvalidProvider);
            }
            Ok(Invocation::Client(ClientCommand::SetEnabled {
                id,
                enabled,
            }))
        }
        _ => Err(CliError::Usage),
    }
}

pub(crate) fn usage() -> &'static str {
    "usage: quotabard [quota [--json] | quota enable <provider> | quota disable <provider>]"
}

/// Read only the configured feed and the credential needed for a validated PUT.
pub(crate) async fn run_from_environment(command: ClientCommand) -> Result<(), CliError> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .unwrap_or_default();
    let feed_url = match env::var_os("QUOTA_FEED") {
        None => None,
        Some(value) => Some(value.into_string().map_err(|_| CliError::InvalidFeedUrl)?),
    };
    let client =
        build_client(REQUEST_TIMEOUT, CONNECT_TIMEOUT).map_err(|_| CliError::DaemonUnavailable)?;
    let output = execute_with(
        command,
        &home,
        feed_url.as_deref(),
        &client,
        load_control_token,
    )
    .await?;
    io::stdout()
        .lock()
        .write_all(&output)
        .map_err(|_| CliError::DaemonUnavailable)
}

/// Execute against a caller-owned client. The loader seam lets tests prove URL
/// and provider validation happen before the read-only token access.
pub(crate) async fn execute_with<F>(
    command: ClientCommand,
    home: &Path,
    configured_feed_url: Option<&str>,
    client: &Client,
    token_loader: F,
) -> Result<Vec<u8>, CliError>
where
    F: FnOnce(&Path) -> Result<ControlToken, RuntimeStateError>,
{
    if let ClientCommand::SetEnabled { id, .. } = &command
        && !valid_provider_id(id)
    {
        return Err(CliError::InvalidProvider);
    }

    // QUOTA_FEED always names the existing GET route. Validate the entire value
    // before deriving a control route, reading a token, or sending a request.
    let base_url = validate_feed_url(configured_feed_url.unwrap_or(DEFAULT_FEED_URL))?;
    match command {
        ClientCommand::Quotas { json } => get_quotas(client, base_url, json).await,
        ClientCommand::SetEnabled { id, enabled } => {
            let mut url = base_url;
            url.set_path(&format!("/providers/{id}/enabled"));
            let token = token_loader(home).map_err(|_| CliError::TokenUnavailable)?;
            set_enabled(client, url, token, id, enabled).await
        }
    }
}

fn validate_feed_url(value: &str) -> Result<Url, CliError> {
    let (_, authority_and_path) = value.split_once("://").ok_or(CliError::InvalidFeedUrl)?;
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let authority_host = authority.rsplit('@').next().unwrap_or_default();
    let raw_host = authority_host
        .split_once(':')
        .map_or(authority_host, |(host, _)| host);
    let exact_loopback = raw_host == "127.0.0.1" || raw_host.eq_ignore_ascii_case("localhost");
    let has_userinfo = authority.contains('@');
    let url = Url::parse(value).map_err(|_| CliError::InvalidFeedUrl)?;
    let parsed_loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    if url.scheme() != "http"
        || !exact_loopback
        || !parsed_loopback
        || has_userinfo
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/quotas"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default().is_none()
    {
        return Err(CliError::InvalidFeedUrl);
    }
    Ok(url)
}

fn valid_provider_id(id: &str) -> bool {
    matches!(id, "claude" | "codex" | "cursor" | "opencode-go" | "muse")
}

async fn get_quotas(client: &Client, url: Url, json: bool) -> Result<Vec<u8>, CliError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| CliError::DaemonUnavailable)?;
    if !response.status().is_success() {
        return Err(server_error(response, MAX_ERROR_BYTES).await);
    }

    let bytes = read_response(response, MAX_FEED_BYTES).await?;
    let feed = decode_feed_v1(&bytes).map_err(|error| match error {
        FeedDecodeError::InvalidPayload => CliError::InvalidFeed,
        FeedDecodeError::UnsupportedSchema => CliError::UnsupportedFeedSchema,
    })?;
    if json {
        Ok(bytes)
    } else {
        let mut output = format_feed(&feed, Utc::now(), None).text.into_bytes();
        output.push(b'\n');
        Ok(output)
    }
}

#[derive(Serialize)]
struct EnabledBody {
    enabled: bool,
}

async fn set_enabled(
    client: &Client,
    url: Url,
    token: ControlToken,
    id: String,
    enabled: bool,
) -> Result<Vec<u8>, CliError> {
    let mut headers = HeaderMap::new();
    token
        .insert_control_header(&mut headers)
        .map_err(|_| CliError::TokenUnavailable)?;
    let response = client
        .put(url)
        .headers(headers)
        .json(&EnabledBody { enabled })
        .send()
        .await
        .map_err(|_| CliError::DaemonUnavailable)?;
    if !response.status().is_success() {
        return Err(server_error(response, MAX_ERROR_BYTES).await);
    }
    let _ = read_response(response, MAX_CONTROL_RESPONSE_BYTES).await?;
    Ok(format!(
        "Provider {id} {}.\n",
        if enabled { "enabled" } else { "disabled" }
    )
    .into_bytes())
}

async fn read_response(response: Response, max_bytes: usize) -> Result<Vec<u8>, CliError> {
    read_bounded_body(response, max_bytes)
        .await
        .map_err(|error| match error {
            crate::feed::ErrorCode::ProviderError => CliError::ResponseTooLarge,
            _ => CliError::DaemonUnavailable,
        })
}

async fn server_error(response: Response, max_bytes: usize) -> CliError {
    let body = match read_response(response, max_bytes).await {
        Ok(body) => body,
        Err(error) => return error,
    };
    let Ok(response) = serde_json::from_slice::<ErrorResponse>(&body) else {
        return CliError::DaemonRejected;
    };
    match response.error_code {
        Some(ServerErrorCode::CapacityUnavailable) => CliError::Busy,
        Some(ServerErrorCode::ConfigurationError) => CliError::Configuration,
        Some(ServerErrorCode::Unauthorized | ServerErrorCode::Forbidden) => CliError::Unauthorized,
        Some(ServerErrorCode::PersistenceError) => CliError::Persistence,
        _ => CliError::DaemonRejected,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResponse {
    error_code: Option<ServerErrorCode>,
}

#[derive(Deserialize)]
enum ServerErrorCode {
    #[serde(rename = "capacity_unavailable")]
    CapacityUnavailable,
    #[serde(rename = "configuration_error")]
    ConfigurationError,
    #[serde(rename = "unauthorized")]
    Unauthorized,
    #[serde(rename = "forbidden")]
    Forbidden,
    #[serde(rename = "persistence_error")]
    PersistenceError,
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::{
        CliError, ClientCommand, Invocation, execute_with, parse_args, usage, validate_feed_url,
    };
    use crate::feed_text::format_feed;
    use crate::runtime_state::{RuntimeState, RuntimeStateError, load_control_token};
    use reqwest::Client;
    use std::ffi::OsString;
    use std::fs;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    const FEED: &[u8] = br#"{"schemaVersion":1,"generatedAt":"2027-01-15T08:00:00Z","disabledProviderIds":[],"providers":[{"id":"claude","name":"Claude","tier":null,"capturedAt":null,"ageSeconds":null,"freshness":"unknown","errorCode":null,"status":"unknown","quotas":[]}] }"#;
    static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

    struct Home(PathBuf);

    impl Home {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "quotabard-cli-{}-{}",
                std::process::id(),
                NEXT_HOME.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write_token(&self) {
            use std::os::unix::fs::PermissionsExt;
            let directory = self.path().join(".quotabar/daemon");
            fs::create_dir_all(&directory).unwrap();
            let token_path = directory.join("control-token");
            fs::write(&token_path, [vec![b'a'; 64], vec![b'\n']].concat()).unwrap();
            fs::set_permissions(token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Request {
        line: String,
        headers: String,
        body: Vec<u8>,
    }

    async fn receive_request(stream: &mut TcpStream) -> Request {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request_line = line.trim_end().to_owned();
        let mut headers = String::new();
        let mut content_length = 0;
        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse::<usize>().unwrap();
            }
            headers.push_str(&line);
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).await.unwrap();
        Request {
            line: request_line,
            headers,
            body,
        }
    }

    fn response(status: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
        let mut bytes = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    async fn serve_once(
        listener: TcpListener,
        status: &'static str,
        body: &'static [u8],
        extra_headers: &'static str,
    ) -> Request {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_request(&mut stream).await;
        stream
            .write_all(&response(status, body, extra_headers))
            .await
            .unwrap();
        request
    }

    fn loopback_listener() -> (TcpListener, SocketAddr) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        (TcpListener::from_std(listener).unwrap(), address)
    }

    fn client() -> Client {
        crate::providers::http::build_client(Duration::from_secs(8), Duration::from_secs(5))
            .unwrap()
    }

    fn url(address: SocketAddr, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", address.port())
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn standard_arguments_select_client_before_daemon_and_reject_unsafe_provider_ids() {
        assert_eq!(
            parse_args(Vec::<OsString>::new()).unwrap(),
            Invocation::Daemon
        );
        assert_eq!(
            parse_args(os(&["quota"])).unwrap(),
            Invocation::Client(ClientCommand::Quotas { json: false })
        );
        assert_eq!(
            parse_args(os(&["quota", "--json"])).unwrap(),
            Invocation::Client(ClientCommand::Quotas { json: true })
        );
        assert_eq!(
            parse_args(os(&["quota", "enable", "muse"])).unwrap(),
            Invocation::Client(ClientCommand::SetEnabled {
                id: "muse".to_owned(),
                enabled: true,
            })
        );
        assert_eq!(
            parse_args(os(&["quota", "disable", "opencode-go"])).unwrap(),
            Invocation::Client(ClientCommand::SetEnabled {
                id: "opencode-go".to_owned(),
                enabled: false,
            })
        );
        assert_eq!(
            parse_args(os(&["quota", "enable", "../muse"])),
            Err(CliError::InvalidProvider)
        );
        assert_eq!(
            parse_args(os(&["quota", "disable", "cursor", "extra"])),
            Err(CliError::Usage)
        );
        assert_eq!(
            usage(),
            "usage: quotabard [quota [--json] | quota enable <provider> | quota disable <provider>]"
        );
    }

    #[test]
    fn only_exact_http_loopback_feed_urls_and_routes_are_accepted() {
        for good in [
            "http://127.0.0.1/quotas",
            "http://localhost/quotas",
            "http://127.0.0.1:18787/quotas",
        ] {
            assert!(validate_feed_url(good).is_ok(), "{good}");
        }
        for bad in [
            "https://127.0.0.1/quotas",
            "http://127.0.0.2/quotas",
            "http://127.1/quotas",
            "http://2130706433/quotas",
            "http://0x7f000001/quotas",
            "http://[::1]/quotas",
            "http://example.invalid/quotas",
            "http://user@127.0.0.1/quotas",
            "http://user:pass@localhost/quotas",
            "http://@localhost/quotas",
            "http://127.0.0.1/quotas?x=1",
            "http://127.0.0.1/quotas#fragment",
            "http://127.0.0.1/other",
            "not a URL",
        ] {
            assert_eq!(
                validate_feed_url(bad),
                Err(CliError::InvalidFeedUrl),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn get_sends_exact_route_without_token_and_json_preserves_validated_bytes() {
        let home = Home::new();
        let (listener, address) = loopback_listener();
        let task = tokio::spawn(serve_once(
            listener,
            "200 OK",
            FEED,
            "Content-Type: application/json\r\n",
        ));
        let loader_calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = Arc::clone(&loader_calls);
        let result = execute_with(
            ClientCommand::Quotas { json: true },
            home.path(),
            Some(&url(address, "/quotas")),
            &client(),
            move |_| {
                observed_calls.fetch_add(1, Ordering::SeqCst);
                Err(RuntimeStateError::InvalidControlToken)
            },
        )
        .await
        .unwrap();
        let request = task.await.unwrap();
        assert_eq!(request.line, "GET /quotas HTTP/1.1");
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains(&format!("host: 127.0.0.1:{}", address.port()))
        );
        assert!(
            !request
                .headers
                .to_ascii_lowercase()
                .contains("x-quotabar-control-token")
        );
        assert!(request.body.is_empty());
        assert_eq!(result, FEED);
        assert_eq!(loader_calls.load(Ordering::SeqCst), 0);
        assert!(!home.path().join(".quotabar").exists());
    }

    #[tokio::test]
    async fn human_get_uses_the_shared_quota_formatter() {
        let (listener, address) = loopback_listener();
        let task = tokio::spawn(serve_once(listener, "200 OK", FEED, ""));
        let output = execute_with(
            ClientCommand::Quotas { json: false },
            Path::new("/synthetic-home"),
            Some(&url(address, "/quotas")),
            &client(),
            |_| panic!("GET must not load the control token"),
        )
        .await
        .unwrap();
        let request = task.await.unwrap();
        assert_eq!(request.line, "GET /quotas HTTP/1.1");
        let feed = crate::feed::decode_feed_v1(FEED).unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2027-01-15T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let expected = format_feed(&feed, now, None).text;
        assert_eq!(output.strip_suffix(b"\n").unwrap(), expected.as_bytes());
    }

    #[tokio::test]
    async fn put_uses_same_loopback_authority_exact_route_json_and_control_token() {
        let home = Home::new();
        home.write_token();
        let (listener, address) = loopback_listener();
        let task = tokio::spawn(serve_once(
            listener,
            "200 OK",
            br#"{"id":"cursor","enabled":false}"#,
            "Content-Type: application/json\r\n",
        ));
        let output = execute_with(
            ClientCommand::SetEnabled {
                id: "cursor".into(),
                enabled: false,
            },
            home.path(),
            Some(&url(address, "/quotas")),
            &client(),
            load_control_token,
        )
        .await
        .unwrap();
        let request = task.await.unwrap();
        assert_eq!(request.line, "PUT /providers/cursor/enabled HTTP/1.1");
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains(&format!("host: 127.0.0.1:{}", address.port()))
        );
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("x-quotabar-control-token: ")
        );
        assert!(request.headers.contains(&"a".repeat(64)));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
            serde_json::json!({"enabled":false})
        );
        assert_eq!(output, b"Provider cursor disabled.\n");
        assert!(!home.path().join(".quotabar/daemon/instance.lock").exists());
        assert!(!home.path().join(".quotabar/daemon.json").exists());
    }

    #[tokio::test]
    async fn invalid_url_and_provider_are_rejected_before_token_read_or_network() {
        let home = Home::new();
        let (listener, address) = loopback_listener();
        let (connection_tx, connection_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let accepted =
                tokio::time::timeout(Duration::from_millis(250), listener.accept()).await;
            let _ = connection_tx.send(accepted.is_ok());
        });
        let token_reads = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&token_reads);
        let invalid_route = execute_with(
            ClientCommand::SetEnabled {
                id: "cursor".into(),
                enabled: true,
            },
            home.path(),
            Some(&url(address, "/wrong")),
            &client(),
            move |_| {
                reads.fetch_add(1, Ordering::SeqCst);
                Err(RuntimeStateError::InvalidControlToken)
            },
        )
        .await;
        assert_eq!(invalid_route, Err(CliError::InvalidFeedUrl));

        let reads = Arc::clone(&token_reads);
        let invalid_id = execute_with(
            ClientCommand::SetEnabled {
                id: "../cursor".into(),
                enabled: true,
            },
            home.path(),
            Some(&url(address, "/quotas")),
            &client(),
            move |_| {
                reads.fetch_add(1, Ordering::SeqCst);
                Err(RuntimeStateError::InvalidControlToken)
            },
        )
        .await;
        assert_eq!(invalid_id, Err(CliError::InvalidProvider));
        assert_eq!(token_reads.load(Ordering::SeqCst), 0);
        assert!(!connection_rx.await.unwrap());
        server.await.unwrap();
        assert!(!home.path().join(".quotabar").exists());
    }

    #[tokio::test]
    async fn malformed_and_oversized_feed_responses_are_safe() {
        let (listener, address) = loopback_listener();
        let body = br#"{"schemaVersion":2,"generatedAt":"2027-01-15T08:00:00Z","disabledProviderIds":[],"providers":[],"secret":"do-not-print"}"#;
        let task = tokio::spawn(serve_once(listener, "200 OK", body, ""));
        let result = execute_with(
            ClientCommand::Quotas { json: true },
            Path::new("/synthetic-home"),
            Some(&url(address, "/quotas")),
            &client(),
            |_| panic!("GET must not read a token"),
        )
        .await;
        task.await.unwrap();
        assert_eq!(result, Err(CliError::UnsupportedFeedSchema));
        assert!(!result.unwrap_err().to_string().contains("do-not-print"));

        let (listener, address) = loopback_listener();
        let large = vec![b' '; super::MAX_FEED_BYTES + 1];
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = receive_request(&mut stream).await;
            // The bounded client closes as soon as it sees the oversized
            // Content-Length, so a broken pipe is the expected loopback result.
            let _ = stream.write_all(&response("200 OK", &large, "")).await;
        });
        let result = execute_with(
            ClientCommand::Quotas { json: false },
            Path::new("/synthetic-home"),
            Some(&url(address, "/quotas")),
            &client(),
            |_| panic!("GET must not read a token"),
        )
        .await;
        task.await.unwrap();
        assert_eq!(result, Err(CliError::ResponseTooLarge));
    }

    #[tokio::test]
    async fn safe_error_codes_are_mapped_without_returning_untrusted_body_text() {
        let (listener, address) = loopback_listener();
        let body = br#"{"errorCode":"capacity_unavailable","secret":"synthetic-secret"}"#;
        let task = tokio::spawn(serve_once(listener, "503 Service Unavailable", body, ""));
        let result = execute_with(
            ClientCommand::Quotas { json: false },
            Path::new("/synthetic-home"),
            Some(&url(address, "/quotas")),
            &client(),
            |_| panic!("GET must not read a token"),
        )
        .await
        .unwrap_err();
        task.await.unwrap();
        assert_eq!(result, CliError::Busy);
        assert!(!result.to_string().contains("synthetic-secret"));
    }

    async fn redirect_test(command: ClientCommand) -> Request {
        let home = Home::new();
        home.write_token();
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination_address = destination.local_addr().unwrap();
        let redirector = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_address = redirector.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = redirector.accept().await.unwrap();
            let request = receive_request(&mut stream).await;
            let response = response(
                "302 Found",
                b"",
                &format!(
                    "Location: http://127.0.0.1:{}/redirect-target\r\n",
                    destination_address.port()
                ),
            );
            stream.write_all(&response).await.unwrap();
            let forwarded = tokio::time::timeout(Duration::from_millis(200), destination.accept())
                .await
                .is_ok();
            (request, forwarded)
        });
        let command_is_get = matches!(command, ClientCommand::Quotas { .. });
        let result = execute_with(
            command,
            home.path(),
            Some(&url(redirect_address, "/quotas")),
            &client(),
            load_control_token,
        )
        .await;
        assert!(result.is_err());
        let (request, forwarded) = server.await.unwrap();
        assert!(
            !forwarded,
            "redirect destination received a follow-on request"
        );
        if command_is_get {
            assert!(
                !request
                    .headers
                    .to_ascii_lowercase()
                    .contains("x-quotabar-control-token")
            );
        } else {
            assert!(request.headers.contains(&"a".repeat(64)));
        }
        request
    }

    #[tokio::test]
    async fn get_and_put_do_not_follow_loopback_redirects_or_forward_control_tokens() {
        let get = redirect_test(ClientCommand::Quotas { json: false }).await;
        assert_eq!(get.line, "GET /quotas HTTP/1.1");
        let put = redirect_test(ClientCommand::SetEnabled {
            id: "muse".into(),
            enabled: true,
        })
        .await;
        assert_eq!(put.line, "PUT /providers/muse/enabled HTTP/1.1");
    }

    #[tokio::test]
    async fn client_dispatch_works_while_another_runtime_state_owner_holds_the_lock() {
        let home = Home::new();
        let state = RuntimeState::open(home.path()).unwrap();
        let (listener, address) = loopback_listener();
        let task = tokio::spawn(serve_once(listener, "200 OK", FEED, ""));
        let invocation = parse_args([OsString::from("quota"), OsString::from("--json")]).unwrap();
        let Invocation::Client(command) = invocation else {
            panic!("client command did not dispatch");
        };
        let output = execute_with(
            command,
            home.path(),
            Some(&url(address, "/quotas")),
            &client(),
            |_| panic!("GET must not read a control token"),
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert_eq!(output, FEED);
        assert!(matches!(
            RuntimeState::open(home.path()),
            Err(RuntimeStateError::AlreadyRunning)
        ));
        assert!(!home.path().join(".quotabar/daemon/control-token").exists());
        drop(state);
        drop(RuntimeState::open(home.path()).unwrap());
    }

    #[test]
    fn readonly_token_loader_rejects_missing_token_without_creating_state() {
        let home = Home::new();
        assert!(load_control_token(home.path()).is_err());
        assert!(!home.path().join(".quotabar").exists());
    }

    #[test]
    fn custom_loopback_port_survives_control_route_derivation() {
        let url = validate_feed_url("http://localhost:18787/quotas").unwrap();
        let mut control = url;
        control.set_path("/providers/opencode-go/enabled");
        assert_eq!(
            control.as_str(),
            "http://localhost:18787/providers/opencode-go/enabled"
        );
    }
}
