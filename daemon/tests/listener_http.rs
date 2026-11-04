use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use quotabard::listener::LoopbackListener;
use quotabard::quota::{Observation, Quota};
use quotabard::runtime_state::RuntimeState;
use quotabard::scheduler::{
    AttemptOutcome, CheckFuture, ProviderCheck, SchedulerOwner, WallNow, start,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Notify, Semaphore, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const PROVIDER_IDS: [&str; 5] = ["claude", "codex", "cursor", "opencode-go", "muse"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(7);

struct TestHome(PathBuf);

impl TestHome {
    fn new() -> Self {
        let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quotabard-listener-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Calls([AtomicUsize; PROVIDER_IDS.len()]);

impl Calls {
    fn new() -> Self {
        Self(std::array::from_fn(|_| AtomicUsize::new(0)))
    }

    fn increment(&self, id: &str) -> usize {
        let index = PROVIDER_IDS
            .iter()
            .position(|provider| *provider == id)
            .unwrap();
        self.0[index].fetch_add(1, Ordering::SeqCst) + 1
    }

    fn count(&self, id: &str) -> usize {
        let index = PROVIDER_IDS
            .iter()
            .position(|provider| *provider == id)
            .unwrap();
        self.0[index].load(Ordering::SeqCst)
    }
}

struct Harness {
    _home: TestHome,
    address: SocketAddr,
    token: String,
    owner: Option<SchedulerOwner>,
    server: Option<JoinHandle<io::Result<()>>>,
    stop: Option<oneshot::Sender<()>>,
}

impl Harness {
    async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            server.await.unwrap().unwrap();
        }
        if let Some(owner) = self.owner.take() {
            owner.shutdown().await;
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.address.port())
    }
}

struct WireResponse {
    status: u16,
    content_type: Option<String>,
    has_cors_header: bool,
    body: String,
}

fn wall_clock(seconds: Arc<AtomicI64>) -> WallNow {
    Arc::new(move || DateTime::from_timestamp(seconds.load(Ordering::SeqCst), 0).unwrap())
}

fn test_observation() -> Observation {
    Observation {
        captured_at: DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap(),
        tier: Some("Offline test".to_owned()),
        quotas: vec![Quota {
            key: "session".to_owned(),
            label: "Session".to_owned(),
            percent_remaining: Some(75.0),
            balance_remaining: None,
            balance_used: None,
            balance_cap: None,
            balance_unit: None,
            units_used: None,
            units_limit: None,
            resets_at: None,
            window_duration_seconds: None,
        }],
    }
}

fn success_check(calls: Arc<Calls>) -> ProviderCheck {
    Arc::new(move |id| {
        calls.increment(id);
        Box::pin(async { AttemptOutcome::success(test_observation()) })
    })
}

async fn harness(check: ProviderCheck, clock: Arc<AtomicI64>, config: Option<&[u8]>) -> Harness {
    let home = TestHome::new();
    let state = RuntimeState::open(&home.0).unwrap();
    let control_token = state.load_or_create_control_token().unwrap();
    if let Some(config) = config {
        fs::write(home.0.join(".quotabar/daemon.json"), config).unwrap();
    }
    let token_path = home.0.join(".quotabar/daemon/control-token");
    let mut token_bytes = fs::read(token_path).unwrap();
    assert_eq!(token_bytes.pop(), Some(b'\n'));
    let token = String::from_utf8(token_bytes).unwrap();
    let (owner, client) = start(state, check, wall_clock(clock));
    let listener = LoopbackListener::bind(0, client, control_token)
        .await
        .unwrap();
    let address = listener.local_addr();
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(listener.serve(async move {
        let _ = stopped.await;
    }));
    Harness {
        _home: home,
        address,
        token,
        owner: Some(owner),
        server: Some(server),
        stop: Some(stop),
    }
}

async fn request(
    address: SocketAddr,
    host: Option<&str>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> WireResponse {
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    if let Some(host) = host {
        request.push_str(&format!("Host: {host}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    request.push_str(std::str::from_utf8(body).unwrap());

    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    match timeout(REQUEST_TIMEOUT, stream.read_to_end(&mut response))
        .await
        .unwrap()
    {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
        Err(error) => panic!("HTTP connection read failed: {}", error.kind()),
    }
    parse_response(&response)
}

async fn raw_request(address: SocketAddr, bytes: &[u8]) -> WireResponse {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(bytes).await.unwrap();
    let mut response = Vec::new();
    match timeout(REQUEST_TIMEOUT, stream.read_to_end(&mut response))
        .await
        .unwrap()
    {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
        Err(error) => panic!("HTTP connection read failed: {}", error.kind()),
    }
    parse_response(&response)
}

async fn chunked_request(address: SocketAddr, host: &str, path: &str, body: &[u8]) -> WireResponse {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    for chunk in body.chunks(16 * 1024) {
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await
            .unwrap();
        stream.write_all(chunk).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
    }
    stream.write_all(b"0\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    match timeout(REQUEST_TIMEOUT, stream.read_to_end(&mut response))
        .await
        .unwrap()
    {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
        Err(error) => panic!("HTTP connection read failed: {}", error.kind()),
    }
    parse_response(&response)
}

fn parse_response(bytes: &[u8]) -> WireResponse {
    let response = String::from_utf8_lossy(bytes);
    let Some((head, body)) = response.split_once("\r\n\r\n") else {
        return WireResponse {
            status: 0,
            content_type: None,
            has_cors_header: false,
            body: String::new(),
        };
    };
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut content_type = None;
    let mut has_cors_header = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_owned());
        }
        has_cors_header |= name.to_ascii_lowercase().starts_with("access-control-");
    }
    WireResponse {
        status,
        content_type,
        has_cors_header,
        body: body.to_owned(),
    }
}

fn assert_error(response: &WireResponse, status: u16, code: &str) {
    assert_eq!(response.status, status, "unexpected HTTP status");
    assert_eq!(response.content_type.as_deref(), Some("application/json"));
    assert!(!response.has_cors_header);
    let value: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(value["errorCode"], code);
    assert_eq!(value.as_object().unwrap().len(), 1);
}

fn control_headers(token: Option<&str>) -> Vec<(&'static str, &str)> {
    token
        .map(|token| vec![("X-QuotaBar-Control-Token", token)])
        .unwrap_or_default()
}

#[tokio::test]
async fn get_quota_uses_real_scheduler_and_enforces_loopback_authorities() {
    let calls = Arc::new(Calls::new());
    let mut server = harness(
        success_check(Arc::clone(&calls)),
        Arc::new(AtomicI64::new(1_800_000_000)),
        None,
    )
    .await;
    let host = server.host();

    for authority in [&host, &format!("localhost:{}", server.address.port())] {
        let response = request(server.address, Some(authority), "GET", "/quotas", &[], b"").await;
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type.as_deref(), Some("application/json"));
        assert!(!response.has_cors_header);
        let feed: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(feed["schemaVersion"], 1);
        assert_eq!(feed["providers"].as_array().unwrap().len(), 5);
        assert_eq!(feed["disabledProviderIds"].as_array().unwrap().len(), 0);
    }
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [1; 5]);

    let wrong_port = server.address.port().wrapping_add(1);
    let response = request(
        server.address,
        Some(&format!("127.0.0.1:{wrong_port}")),
        "GET",
        "/quotas",
        &[],
        b"",
    )
    .await;
    assert_error(&response, 403, "forbidden");
    let response = request(
        server.address,
        Some(&host),
        "GET",
        "/quotas",
        &[("Origin", "")],
        b"",
    )
    .await;
    assert_error(&response, 403, "forbidden");
    let response = request(server.address, None, "GET", "/quotas", &[], b"").await;
    assert_error(&response, 403, "forbidden");
    server.stop().await;
}

#[tokio::test]
async fn control_route_authenticates_and_validates_exact_json_shape() {
    let calls = Arc::new(Calls::new());
    let mut server = harness(
        success_check(calls),
        Arc::new(AtomicI64::new(1_800_000_000)),
        None,
    )
    .await;
    let host = server.host();

    let response = request(
        server.address,
        Some(&host),
        "PUT",
        "/providers/muse/enabled",
        &[],
        br#"{"enabled":false}"#,
    )
    .await;
    assert_error(&response, 401, "unauthorized");

    let response = request(
        server.address,
        Some(&host),
        "PUT",
        "/providers/muse/enabled",
        &control_headers(Some("not-the-control-token")),
        br#"{"enabled":false}"#,
    )
    .await;
    assert_error(&response, 401, "unauthorized");

    let auth = control_headers(Some(&server.token));
    for body in [
        br#"{"enabled":1}"#.as_slice(),
        br#"{"enabled":false,"other":true}"#.as_slice(),
        br#"{"enabled":false} trailing"#.as_slice(),
    ] {
        let response = request(
            server.address,
            Some(&host),
            "PUT",
            "/providers/muse/enabled",
            &auth,
            body,
        )
        .await;
        assert_error(&response, 400, "invalid_request");
    }

    let response = request(
        server.address,
        Some(&host),
        "PUT",
        "/providers/not-a-provider/enabled",
        &auth,
        br#"{"enabled":false}"#,
    )
    .await;
    assert_error(&response, 400, "invalid_request");

    let response = request(
        server.address,
        Some(&host),
        "PUT",
        "/providers/muse/enabled",
        &auth,
        &vec![b' '; 1025],
    )
    .await;
    assert_error(&response, 413, "request_too_large");
    server.stop().await;
}

#[tokio::test]
async fn control_mutation_applies_and_hides_cached_provider_immediately() {
    let calls = Arc::new(Calls::new());
    let mut server = harness(
        success_check(Arc::clone(&calls)),
        Arc::new(AtomicI64::new(1_800_000_000)),
        None,
    )
    .await;
    let host = server.host();
    let initial = request(server.address, Some(&host), "GET", "/quotas", &[], b"").await;
    assert_eq!(initial.status, 200);

    let response = request(
        server.address,
        Some(&host),
        "PUT",
        "/providers/muse/enabled",
        &control_headers(Some(&server.token)),
        br#"{"enabled":false}"#,
    )
    .await;
    assert_eq!(response.status, 200);
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["id"], "muse");
    assert_eq!(body["enabled"], false);

    let feed = request(server.address, Some(&host), "GET", "/quotas", &[], b"").await;
    assert_eq!(feed.status, 200);
    let feed: serde_json::Value = serde_json::from_str(&feed.body).unwrap();
    assert!(
        feed["disabledProviderIds"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("muse"))
    );
    assert!(
        !feed["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|provider| provider["id"] == "muse")
    );
    assert_eq!(calls.count("muse"), 1);
    server.stop().await;
}

#[tokio::test]
async fn malformed_configuration_fails_closed_without_provider_calls_or_overwrite() {
    let calls = Arc::new(Calls::new());
    let invalid = br#"{"disabledProviders":["unknown"]}"#;
    let mut server = harness(
        success_check(Arc::clone(&calls)),
        Arc::new(AtomicI64::new(1_800_000_000)),
        Some(invalid),
    )
    .await;
    let host = server.host();

    let response = request(server.address, Some(&host), "GET", "/quotas", &[], b"").await;
    assert_error(&response, 503, "configuration_error");
    let response = request(
        server.address,
        Some(&host),
        "PUT",
        "/providers/muse/enabled",
        &control_headers(Some(&server.token)),
        br#"{"enabled":false}"#,
    )
    .await;
    assert_error(&response, 503, "configuration_error");
    assert_eq!(
        fs::read(server._home.0.join(".quotabar/daemon.json")).unwrap(),
        invalid
    );
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [0; 5]);
    server.stop().await;
}

#[tokio::test]
async fn disabled_inflight_provider_result_is_discarded_by_control_route() {
    let calls = Arc::new(Calls::new());
    let clock = Arc::new(AtomicI64::new(1_800_000_000));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let check = {
        let calls = Arc::clone(&calls);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        Arc::new(move |id: &'static str| {
            let attempt = calls.increment(id);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            Box::pin(async move {
                if id == "muse" && attempt == 2 {
                    started.notify_one();
                    release.notified().await;
                }
                AttemptOutcome::success(test_observation())
            }) as CheckFuture
        }) as ProviderCheck
    };
    let mut server = harness(check, Arc::clone(&clock), None).await;
    let host = server.host();
    let initial = request(server.address, Some(&host), "GET", "/quotas", &[], b"").await;
    assert_eq!(initial.status, 200);
    clock.fetch_add(301, Ordering::SeqCst);

    let pending_address = server.address;
    let pending_host = host.clone();
    let pending = tokio::spawn(async move {
        request(
            pending_address,
            Some(&pending_host),
            "GET",
            "/quotas",
            &[],
            b"",
        )
        .await
    });
    started.notified().await;

    let response = request(
        server.address,
        Some(&host),
        "PUT",
        "/providers/muse/enabled",
        &control_headers(Some(&server.token)),
        br#"{"enabled":false}"#,
    )
    .await;
    assert_eq!(response.status, 200);
    let feed = request(server.address, Some(&host), "GET", "/quotas", &[], b"").await;
    assert_eq!(feed.status, 200);
    let feed: serde_json::Value = serde_json::from_str(&feed.body).unwrap();
    assert!(
        feed["disabledProviderIds"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("muse"))
    );
    assert!(
        !feed["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|provider| provider["id"] == "muse")
    );

    release.notify_one();
    let pending = pending.await.unwrap();
    assert_eq!(pending.status, 200);
    let feed: serde_json::Value = serde_json::from_str(&pending.body).unwrap();
    assert!(
        !feed["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|provider| provider["id"] == "muse")
    );
    assert_eq!(calls.count("muse"), 2);
    server.stop().await;
}

#[tokio::test]
async fn get_deadline_returns_busy_feed_without_cancelling_shared_provider_work() {
    let calls = Arc::new(Calls::new());
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let check = {
        let calls = Arc::clone(&calls);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        Arc::new(move |id: &'static str| {
            calls.increment(id);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            Box::pin(async move {
                if id == "claude" {
                    started.notify_one();
                    release.notified().await;
                }
                AttemptOutcome::success(test_observation())
            }) as CheckFuture
        }) as ProviderCheck
    };
    let mut server = harness(check, Arc::new(AtomicI64::new(1_800_000_000)), None).await;
    let host = server.host();
    let started_at = Instant::now();
    let pending_address = server.address;
    let pending_host = host.clone();
    let request_task = tokio::spawn(async move {
        request(
            pending_address,
            Some(&pending_host),
            "GET",
            "/quotas",
            &[],
            b"",
        )
        .await
    });
    started.notified().await;
    let response = timeout(Duration::from_millis(4_900), request_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status, 200);
    assert!(started_at.elapsed() < Duration::from_secs(5));
    let feed: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(
        feed["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "claude")
            .unwrap()["errorCode"],
        "busy"
    );
    assert_eq!(calls.count("claude"), 1);

    release.notify_one();
    let feed = request(server.address, Some(&host), "GET", "/quotas", &[], b"").await;
    assert_eq!(feed.status, 200);
    let feed: serde_json::Value = serde_json::from_str(&feed.body).unwrap();
    assert_eq!(
        feed["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "claude")
            .unwrap()["status"],
        "healthy"
    );
    assert_eq!(calls.count("claude"), 1);
    server.stop().await;
}

#[tokio::test]
async fn bounded_connections_handlers_and_http_limits_are_enforced() {
    let calls = Arc::new(Calls::new());
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let check = {
        let calls = Arc::clone(&calls);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        Arc::new(move |id: &'static str| {
            calls.increment(id);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            Box::pin(async move {
                started.notify_one();
                release.acquire_owned().await.unwrap().forget();
                AttemptOutcome::success(test_observation())
            }) as CheckFuture
        }) as ProviderCheck
    };
    let mut server = harness(check, Arc::new(AtomicI64::new(1_800_000_000)), None).await;
    let host = server.host();

    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..17 {
        let address = server.address;
        let host = host.clone();
        requests
            .spawn(async move { request(address, Some(&host), "GET", "/quotas", &[], b"").await });
    }
    started.notified().await;
    let mut capacity_response = false;
    while let Some(result) = requests.join_next().await {
        let response = result.unwrap();
        if response.status == 503 {
            assert_error(&response, 503, "capacity_unavailable");
            capacity_response = true;
            break;
        }
        assert_ne!(response.status, 0, "handler unexpectedly closed a request");
        assert_eq!(response.status, 200);
    }
    assert!(capacity_response);
    release.add_permits(PROVIDER_IDS.len());
    while let Some(result) = requests.join_next().await {
        let response = result.unwrap();
        assert_eq!(response.status, 200);
    }

    let mut sockets = Vec::new();
    for _ in 0..32 {
        let mut stream = TcpStream::connect(server.address).await.unwrap();
        stream
            .write_all(format!("GET /quotas HTTP/1.1\r\nHost: {}\r\nX-Pending:", host).as_bytes())
            .await
            .unwrap();
        sockets.push(stream);
    }
    let mut overflow = TcpStream::connect(server.address).await.unwrap();
    overflow
        .write_all(format!("GET /quotas HTTP/1.1\r\nHost: {}\r\n\r\n", host).as_bytes())
        .await
        .unwrap();
    let mut overflow_response = Vec::new();
    let closed = timeout(
        Duration::from_secs(2),
        overflow.read_to_end(&mut overflow_response),
    )
    .await
    .unwrap();
    if let Err(error) = closed {
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }
    assert!(overflow_response.is_empty());
    drop(sockets);

    let oversized_header = "x".repeat(17 * 1024);
    let response = request(
        server.address,
        Some(&host),
        "GET",
        "/quotas",
        &[("X-Fill", &oversized_header)],
        b"",
    )
    .await;
    assert!(response.status == 0 || response.status == 431);
    server.stop().await;
}

#[tokio::test]
async fn slow_header_and_body_are_bounded_by_one_total_request_deadline() {
    let calls = Arc::new(Calls::new());
    let mut server = harness(
        success_check(calls),
        Arc::new(AtomicI64::new(1_800_000_000)),
        None,
    )
    .await;
    let host = server.host();

    let header_started = Instant::now();
    let mut header_stream = TcpStream::connect(server.address).await.unwrap();
    header_stream
        .write_all(
            format!(
                "POST /hooks/prompt HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nX-Wait:"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut header_response = Vec::new();
    timeout(
        Duration::from_millis(2_500),
        header_stream.read_to_end(&mut header_response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(header_response.is_empty());
    assert!(header_started.elapsed() < Duration::from_millis(2_500));

    let body_started = Instant::now();
    let mut body_stream = TcpStream::connect(server.address).await.unwrap();
    body_stream
        .write_all(
            format!(
                "PUT /providers/muse/enabled HTTP/1.1\r\nHost: {host}\r\nX-QuotaBar-Control-Token: {}\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{{\"enabled\":",
                server.token
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut body_response = Vec::new();
    timeout(
        Duration::from_secs(6),
        body_stream.read_to_end(&mut body_response),
    )
    .await
    .unwrap()
    .unwrap();
    let response = parse_response(&body_response);
    assert_error(&response, 503, "capacity_unavailable");
    assert!(body_started.elapsed() < Duration::from_millis(5_500));
    server.stop().await;
}

#[tokio::test]
async fn hooks_share_the_scheduler_return_official_envelopes_and_prompt_is_cached_only() {
    let calls = Arc::new(Calls::new());
    let mut server = harness(
        success_check(Arc::clone(&calls)),
        Arc::new(AtomicI64::new(1_800_000_000)),
        None,
    )
    .await;
    let host = server.host();
    let start = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/session-start",
        &[("Content-Type", "application/json; charset=utf-8")],
        br#"{"session_id":"abc123","prompt":"synthetic private prompt"}"#,
    )
    .await;
    assert_eq!(start.status, 200);
    assert_eq!(start.content_type.as_deref(), Some("application/json"));
    let start: serde_json::Value = serde_json::from_str(&start.body).unwrap();
    assert_eq!(start["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(
        start["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Quota headroom (QuotaBar)")
    );
    assert!(!start.to_string().contains("synthetic private prompt"));
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [1; 5]);

    let prompt = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"abc123","prompt":"secret prompt"}"#,
    )
    .await;
    assert_eq!(prompt.status, 200);
    assert_eq!(prompt.body, "{}");
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [1; 5]);
    server.stop().await;
}

#[tokio::test]
async fn prompt_reports_worsening_once_rearms_after_improvement_and_never_probes() {
    let calls = Arc::new(Calls::new());
    let clock = Arc::new(AtomicI64::new(1_800_000_000));
    let percent = Arc::new(AtomicUsize::new(75));
    let check = {
        let calls = Arc::clone(&calls);
        let percent = Arc::clone(&percent);
        Arc::new(move |id: &'static str| {
            calls.increment(id);
            let percent = percent.load(Ordering::SeqCst) as f64;
            let quotas = if id == "claude" {
                vec![Quota {
                    key: "session".to_owned(),
                    label: "Session".to_owned(),
                    percent_remaining: Some(percent),
                    balance_remaining: None,
                    balance_used: None,
                    balance_cap: None,
                    balance_unit: None,
                    units_used: None,
                    units_limit: None,
                    resets_at: None,
                    window_duration_seconds: None,
                }]
            } else {
                Vec::new()
            };
            Box::pin(async move {
                AttemptOutcome::success(Observation {
                    captured_at: DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap(),
                    tier: None,
                    quotas,
                })
            }) as CheckFuture
        }) as ProviderCheck
    };
    let mut server = harness(check, Arc::clone(&clock), None).await;
    let host = server.host();
    let start = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/session-start",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"opaque-session"}"#,
    )
    .await;
    assert_eq!(start.status, 200);
    assert_eq!(calls.count("claude"), 1);

    percent.store(40, Ordering::SeqCst);
    let prompt = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"opaque-session"}"#,
    )
    .await;
    assert_eq!(prompt.body, "{}", "prompt must not refresh stale cache");
    assert_eq!(calls.count("claude"), 1);

    clock.fetch_add(301, Ordering::SeqCst);
    let refresh = request(server.address, Some(&host), "GET", "/quotas", &[], b"").await;
    assert_eq!(refresh.status, 200);
    let warning = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"opaque-session"}"#,
    )
    .await;
    assert_eq!(warning.status, 200);
    let warning: serde_json::Value = serde_json::from_str(&warning.body).unwrap();
    assert_eq!(
        warning["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert!(
        warning["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("[warning]")
    );
    assert_eq!(calls.count("claude"), 2);
    assert_eq!(
        request(
            server.address,
            Some(&host),
            "POST",
            "/hooks/prompt",
            &[("Content-Type", "application/json")],
            br#"{"session_id":"opaque-session"}"#,
        )
        .await
        .body,
        "{}"
    );

    percent.store(75, Ordering::SeqCst);
    clock.fetch_add(301, Ordering::SeqCst);
    assert_eq!(
        request(server.address, Some(&host), "GET", "/quotas", &[], b"")
            .await
            .status,
        200
    );
    assert_eq!(
        request(
            server.address,
            Some(&host),
            "POST",
            "/hooks/prompt",
            &[("Content-Type", "application/json")],
            br#"{"session_id":"opaque-session"}"#,
        )
        .await
        .body,
        "{}"
    );

    percent.store(40, Ordering::SeqCst);
    clock.fetch_add(301, Ordering::SeqCst);
    assert_eq!(
        request(server.address, Some(&host), "GET", "/quotas", &[], b"")
            .await
            .status,
        200
    );
    let warning_again = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"opaque-session"}"#,
    )
    .await;
    assert!(warning_again.body.contains("[warning]"));
    assert_eq!(calls.count("claude"), 4);
    server.stop().await;
}

#[tokio::test]
async fn hooks_validate_json_and_content_type_without_echoing_request_data() {
    let calls = Arc::new(Calls::new());
    let mut server = harness(
        success_check(Arc::clone(&calls)),
        Arc::new(AtomicI64::new(1_800_000_000)),
        None,
    )
    .await;
    let host = server.host();
    let malformed = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"x","prompt":"do not echo","broken"#,
    )
    .await;
    assert_error(&malformed, 400, "invalid_request");
    assert!(!malformed.body.contains("do not echo"));
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [0; 5]);

    let wrong_type = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "text/plain")],
        br#"{"session_id":"x"}"#,
    )
    .await;
    assert_error(&wrong_type, 400, "invalid_request");

    let invalid_prompt = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":42}"#,
    )
    .await;
    assert_eq!(invalid_prompt.status, 200);
    assert_eq!(invalid_prompt.body, "{}");

    let wrong_method = request(
        server.address,
        Some(&host),
        "GET",
        "/hooks/prompt",
        &[],
        b"",
    )
    .await;
    assert_error(&wrong_method, 405, "method_not_allowed");
    let origin = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json"), ("Origin", "")],
        br#"{"session_id":"opaque"}"#,
    )
    .await;
    assert_error(&origin, 403, "forbidden");
    let wrong_host = request(
        server.address,
        Some("127.0.0.1:1"),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"opaque"}"#,
    )
    .await;
    assert_error(&wrong_host, 403, "forbidden");

    let invalid_start = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/session-start",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"   "}"#,
    )
    .await;
    assert_eq!(invalid_start.status, 200);
    let output: serde_json::Value = serde_json::from_str(&invalid_start.body).unwrap();
    assert_eq!(
        output["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [1; 5]);
    server.stop().await;
}

#[tokio::test]
async fn hook_body_cap_is_one_mib_while_ordinary_and_control_caps_stay_one_kib() {
    let calls = Arc::new(Calls::new());
    let mut server = harness(
        success_check(Arc::clone(&calls)),
        Arc::new(AtomicI64::new(1_800_000_000)),
        None,
    )
    .await;
    let host = server.host();
    let larger_than_ordinary = format!(
        r#"{{"session_id":"opaque","prompt":"{}"}}"#,
        "p".repeat(2 * 1024)
    );
    let accepted = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        larger_than_ordinary.as_bytes(),
    )
    .await;
    assert_eq!(accepted.status, 200);
    assert_eq!(accepted.body, "{}");
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [0; 5]);

    let ordinary = request(
        server.address,
        Some(&host),
        "GET",
        "/quotas",
        &[],
        &[b'x'; 1025],
    )
    .await;
    assert_error(&ordinary, 413, "request_too_large");
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [0; 5]);

    let declared = raw_request(
        server.address,
        format!(
            "POST /hooks/prompt HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            1024 * 1024 + 1
        )
        .as_bytes(),
    )
    .await;
    assert_error(&declared, 413, "request_too_large");

    let streamed = format!(
        r#"{{"session_id":"opaque","prompt":"{}"}}"#,
        "p".repeat(1024 * 1024)
    );
    let streamed =
        chunked_request(server.address, &host, "/hooks/prompt", streamed.as_bytes()).await;
    assert_error(&streamed, 413, "request_too_large");
    server.stop().await;
}

#[tokio::test]
async fn session_start_wait_is_bounded_and_hook_total_deadline_includes_slow_body() {
    let calls = Arc::new(Calls::new());
    let check = {
        let calls = Arc::clone(&calls);
        Arc::new(move |id| {
            calls.increment(id);
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(1_800)).await;
                AttemptOutcome::success(test_observation())
            }) as CheckFuture
        }) as ProviderCheck
    };
    let mut server = harness(check, Arc::new(AtomicI64::new(1_800_000_000)), None).await;
    let host = server.host();
    let began = Instant::now();
    let start = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/session-start",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"slow-check"}"#,
    )
    .await;
    let elapsed = began.elapsed();
    assert_eq!(start.status, 200);
    assert!(elapsed >= Duration::from_millis(1_350));
    assert!(elapsed < Duration::from_secs(2), "hook took {elapsed:?}");
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [1; 5]);

    let body_start = Instant::now();
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    stream
        .write_all(
            format!(
                "POST /hooks/prompt HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 128\r\nConnection: close\r\n\r\n{{\"session_id\":"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    timeout(
        Duration::from_millis(2_200),
        stream.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    let elapsed = body_start.elapsed();
    assert!(elapsed < Duration::from_millis(2_200));
    assert!(response.is_empty() || parse_response(&response).status == 503);
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [1; 5]);
    server.stop().await;
}

#[tokio::test]
async fn prompt_reloads_configuration_but_never_starts_provider_work() {
    let calls = Arc::new(Calls::new());
    let mut server = harness(
        success_check(Arc::clone(&calls)),
        Arc::new(AtomicI64::new(1_800_000_000)),
        None,
    )
    .await;
    let host = server.host();
    let mut invalid_sessions = vec![
        br#"{}"#.to_vec(),
        br#"{"session_id":null}"#.to_vec(),
        br#"{"session_id":42}"#.to_vec(),
        br#"{"session_id":true}"#.to_vec(),
        br#"{"session_id":[]}"#.to_vec(),
        br#"{"session_id":{}}"#.to_vec(),
        br#"{"session_id":""}"#.to_vec(),
        br#"{"session_id":"   "}"#.to_vec(),
        br#"{"session_id":"\u0000"}"#.to_vec(),
    ];
    invalid_sessions.push(
        serde_json::json!({"session_id": "x".repeat(257)})
            .to_string()
            .into_bytes(),
    );
    for body in &invalid_sessions {
        let response = request(
            server.address,
            Some(&host),
            "POST",
            "/hooks/prompt",
            &[("Content-Type", "application/json")],
            body,
        )
        .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{}");
    }
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [0; 5]);

    let initial = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/session-start",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"config-boundary"}"#,
    )
    .await;
    assert_eq!(initial.status, 200);
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [1; 5]);
    fs::write(
        server._home.0.join(".quotabar/daemon.json"),
        br#"{"disabledProviders":["claude"]}"#,
    )
    .unwrap();
    let disabled = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"config-boundary"}"#,
    )
    .await;
    assert_eq!(disabled.status, 200);
    assert_eq!(disabled.body, "{}");
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [1; 5]);

    fs::write(server._home.0.join(".quotabar/daemon.json"), b"{malformed").unwrap();
    for body in &invalid_sessions {
        let response = request(
            server.address,
            Some(&host),
            "POST",
            "/hooks/prompt",
            &[("Content-Type", "application/json")],
            body,
        )
        .await;
        assert_error(&response, 503, "configuration_error");
    }
    let malformed_json = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"broken""#,
    )
    .await;
    assert_error(&malformed_json, 400, "invalid_request");
    let prompt = request(
        server.address,
        Some(&host),
        "POST",
        "/hooks/prompt",
        &[("Content-Type", "application/json")],
        br#"{"session_id":"config-boundary"}"#,
    )
    .await;
    assert_error(&prompt, 503, "configuration_error");
    assert_eq!(PROVIDER_IDS.map(|id| calls.count(id)), [1; 5]);
    server.stop().await;
}

#[tokio::test]
async fn unknown_routes_and_methods_return_json_errors() {
    let mut server = harness(
        success_check(Arc::new(Calls::new())),
        Arc::new(AtomicI64::new(1_800_000_000)),
        None,
    )
    .await;
    let host = server.host();
    let response = request(server.address, Some(&host), "GET", "/missing", &[], b"").await;
    assert_error(&response, 404, "not_found");
    let response = request(server.address, Some(&host), "POST", "/quotas", &[], b"").await;
    assert_error(&response, 405, "method_not_allowed");
    let response = request(
        server.address,
        Some(&host),
        "PUT",
        "/providers/muse/extra/enabled",
        &[],
        br#"{"enabled":false}"#,
    )
    .await;
    assert_error(&response, 404, "not_found");
    server.stop().await;
}
