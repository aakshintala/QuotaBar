use std::error::Error;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{Extension, Path, Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{HeaderName, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension as AddExtension, Json, Router};
use http_body_util::LengthLimitError;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout};

use crate::hooks::{HookError, HookService};
use crate::runtime_state::ControlToken;
use crate::scheduler::{RequestError, SchedulerClient};

const MAX_CONNECTIONS: usize = 32;
const MAX_HANDLERS: usize = 16;
const MAX_HEADER_BUFFER_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BODY_BYTES: usize = 1024;
const MAX_HOOK_BODY_BYTES: usize = 1024 * 1024;
const HEADER_READ_DEADLINE: Duration = Duration::from_secs(2);
const HOOK_REQUEST_DEADLINE: Duration = Duration::from_secs(2);
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);
const RESPONSE_WRITE_RESERVE: Duration = Duration::from_millis(50);
const REFRESH_WAIT: Duration = Duration::from_millis(4_500);
const CONTROL_TOKEN_HEADER: HeaderName = HeaderName::from_static("x-quotabar-control-token");

#[derive(Debug)]
pub enum ListenerError {
    Bind(io::Error),
    LocalAddress(io::Error),
}

impl std::fmt::Display for ListenerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind(_) => formatter.write_str("could not bind the loopback HTTP listener"),
            Self::LocalAddress(_) => formatter.write_str("could not inspect the listener address"),
        }
    }
}

impl Error for ListenerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bind(error) | Self::LocalAddress(error) => Some(error),
        }
    }
}

/// An IPv4-loopback-only HTTP/1 listener. A port of zero is supported for tests.
pub struct LoopbackListener {
    listener: TcpListener,
    address: SocketAddr,
    app: Router,
    connection_slots: Arc<Semaphore>,
}

impl LoopbackListener {
    pub async fn bind(
        port: u16,
        scheduler: SchedulerClient,
        token: ControlToken,
    ) -> Result<Self, ListenerError> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .await
            .map_err(ListenerError::Bind)?;
        let address = listener.local_addr().map_err(ListenerError::LocalAddress)?;
        let state = Arc::new(AppState {
            hooks: HookService::new(scheduler.clone()),
            scheduler,
            token,
            port: address.port(),
            handler_slots: Arc::new(Semaphore::new(MAX_HANDLERS)),
        });
        let app = Router::new()
            .route("/quotas", get(get_quotas))
            .route("/hooks/session-start", post(hook_session_start))
            .route("/hooks/prompt", post(hook_prompt))
            .route("/providers/{id}/enabled", put(set_enabled))
            .fallback(not_found)
            .method_not_allowed_fallback(method_not_allowed)
            .layer(middleware::from_fn_with_state(state.clone(), request_guard))
            .with_state(state);

        Ok(Self {
            listener,
            address,
            app,
            connection_slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Serve until shutdown, then cancel and join every connection task.
    pub async fn serve<F>(self, shutdown: F) -> io::Result<()>
    where
        F: Future<Output = ()> + Send,
    {
        let Self {
            listener,
            app,
            connection_slots,
            ..
        } = self;
        tokio::pin!(shutdown);
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let accepted_at = Instant::now();
                    let Ok(slot) = Arc::clone(&connection_slots).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let (deadline_signal, deadline) =
                        watch::channel(accepted_at + HEADER_READ_DEADLINE);
                    let service = TowerToHyperService::new(
                        app.clone()
                            .layer(AddExtension(accepted_at))
                            .layer(AddExtension(deadline_signal)),
                    );
                    connections.spawn(async move {
                        let _slot = slot;
                        serve_connection(stream, service, deadline).await;
                    });
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }
}

async fn serve_connection(
    stream: TcpStream,
    service: TowerToHyperService<Router>,
    mut deadline: watch::Receiver<Instant>,
) {
    let mut builder = http1::Builder::new();
    builder
        // The outer absolute deadline covers header parsing and Hyper's actual
        // response writes. The header timer is also measured from acceptance.
        .keep_alive(false)
        .max_buf_size(MAX_HEADER_BUFFER_BYTES)
        .header_read_timeout(HEADER_READ_DEADLINE)
        .timer(TokioTimer::new());
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    loop {
        let deadline_at = *deadline.borrow_and_update();
        tokio::select! {
            _ = sleep_until(deadline_at) => break,
            result = &mut connection => {
                let _ = result;
                return;
            }
            changed = deadline.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
}

struct AppState {
    hooks: HookService,
    scheduler: SchedulerClient,
    token: ControlToken,
    port: u16,
    handler_slots: Arc<Semaphore>,
}

async fn request_guard(
    State(state): State<Arc<AppState>>,
    Extension(accepted_at): Extension<Instant>,
    Extension(deadline_signal): Extension<watch::Sender<Instant>>,
    request: Request,
    next: Next,
) -> Response {
    let request_deadline = accepted_at
        + if is_hook_request(&request) {
            HOOK_REQUEST_DEADLINE
        } else {
            REQUEST_DEADLINE
        };
    deadline_signal.send_replace(request_deadline);

    if !host_is_allowed(&request, state.port) || request.headers().contains_key(ORIGIN) {
        return error_response(StatusCode::FORBIDDEN, "forbidden");
    }
    if is_control_request(&request) {
        let Some(token) = request.headers().get(&CONTROL_TOKEN_HEADER) else {
            return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
        };
        if !state.token.matches(token.as_bytes()) {
            return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
        }
    }
    let Ok(_slot) = Arc::clone(&state.handler_slots).try_acquire_owned() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "capacity_unavailable");
    };
    let body_limit = if is_hook_request(&request) {
        MAX_HOOK_BODY_BYTES
    } else {
        MAX_REQUEST_BODY_BYTES
    };
    if content_length_exceeds(&request, body_limit) {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large");
    }
    let remaining = request_deadline
        .saturating_duration_since(Instant::now())
        .saturating_sub(RESPONSE_WRITE_RESERVE);
    if remaining.is_zero() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "capacity_unavailable");
    }
    match timeout(remaining, async {
        let (parts, body) = request.into_parts();
        let bytes = match to_bytes(body, body_limit).await {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .source()
                    .is_some_and(|source| source.is::<LengthLimitError>()) =>
            {
                return error_response(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large");
            }
            Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
        };
        let request = Request::from_parts(parts, Body::from(bytes));
        next.run(request).await
    })
    .await
    {
        Ok(response) => response,
        Err(_) => error_response(StatusCode::SERVICE_UNAVAILABLE, "capacity_unavailable"),
    }
}

fn is_hook_request(request: &Request) -> bool {
    request.method() == Method::POST
        && matches!(
            request.uri().path(),
            "/hooks/session-start" | "/hooks/prompt"
        )
}

fn content_length_exceeds(request: &Request, limit: usize) -> bool {
    request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
}

fn is_control_request(request: &Request) -> bool {
    if request.method() != axum::http::Method::PUT {
        return false;
    }
    let Some(id) = request
        .uri()
        .path()
        .strip_prefix("/providers/")
        .and_then(|path| path.strip_suffix("/enabled"))
    else {
        return false;
    };
    !id.is_empty() && !id.contains('/')
}

fn host_is_allowed(request: &Request, port: u16) -> bool {
    let Some(host) = request.headers().get(HOST) else {
        return false;
    };
    let loopback = format!("127.0.0.1:{port}");
    let localhost = format!("localhost:{port}");
    host.as_bytes() == loopback.as_bytes() || host.as_bytes() == localhost.as_bytes()
}

async fn get_quotas(State(state): State<Arc<AppState>>) -> Response {
    match state.scheduler.refresh(REFRESH_WAIT).await {
        Ok(feed) => Json(feed).into_response(),
        Err(error) => scheduler_error(error),
    }
}

async fn hook_session_start(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if !is_json_content_type(&request) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let body = match to_bytes(request.into_body(), MAX_HOOK_BODY_BYTES).await {
        Ok(body) => body,
        Err(error)
            if error
                .source()
                .is_some_and(|source| source.is::<LengthLimitError>()) =>
        {
            return error_response(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large");
        }
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    match state.hooks.session_start(body).await {
        Ok(response) => Json(response).into_response(),
        Err(HookError::InvalidJson) => error_response(StatusCode::BAD_REQUEST, "invalid_request"),
        Err(HookError::Scheduler(error)) => scheduler_error(error),
    }
}

async fn hook_prompt(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if !is_json_content_type(&request) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let body = match to_bytes(request.into_body(), MAX_HOOK_BODY_BYTES).await {
        Ok(body) => body,
        Err(error)
            if error
                .source()
                .is_some_and(|source| source.is::<LengthLimitError>()) =>
        {
            return error_response(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large");
        }
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    match state.hooks.prompt(body).await {
        Ok(response) => Json(response).into_response(),
        Err(HookError::InvalidJson) => error_response(StatusCode::BAD_REQUEST, "invalid_request"),
        Err(HookError::Scheduler(error)) => scheduler_error(error),
    }
}

fn is_json_content_type(request: &Request) -> bool {
    request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

async fn set_enabled(
    State(state): State<Arc<AppState>>,
    id: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let id = match id {
        Ok(Path(id)) => id,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    let bytes = match to_bytes(request.into_body(), MAX_REQUEST_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(error)
            if error
                .source()
                .is_some_and(|source| source.is::<LengthLimitError>()) =>
        {
            return error_response(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large");
        }
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    let body: EnabledBody = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    match state.scheduler.set_enabled(id.clone(), body.enabled).await {
        Ok(()) => Json(EnabledResponse {
            id,
            enabled: body.enabled,
        })
        .into_response(),
        Err(error) => scheduler_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledBody {
    enabled: bool,
}

#[derive(Serialize)]
struct EnabledResponse {
    id: String,
    enabled: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    error_code: &'static str,
}

fn error_response(status: StatusCode, code: &'static str) -> Response {
    (
        status,
        [(CONTENT_TYPE, "application/json")],
        Json(ErrorBody { error_code: code }),
    )
        .into_response()
}

fn scheduler_error(error: RequestError) -> Response {
    let (status, code) = match error {
        RequestError::Busy | RequestError::Deadline => {
            (StatusCode::SERVICE_UNAVAILABLE, "capacity_unavailable")
        }
        RequestError::Configuration => (StatusCode::SERVICE_UNAVAILABLE, "configuration_error"),
        RequestError::InvalidProvider => (StatusCode::BAD_REQUEST, "invalid_request"),
        RequestError::Persistence => (StatusCode::SERVICE_UNAVAILABLE, "persistence_error"),
        RequestError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "internal_error"),
    };
    error_response(status, code)
}

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found")
}

async fn method_not_allowed() -> Response {
    error_response(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed")
}

#[cfg(test)]
mod tests {
    use super::{HOOK_REQUEST_DEADLINE, host_is_allowed, serve_connection};
    use axum::http::Request;
    use axum::http::header::{HOST, ORIGIN};
    use axum::routing::get;
    use axum::{Router, body::Body};
    use hyper_util::service::TowerToHyperService;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Notify, watch};
    use tokio::time::{Instant, timeout};

    #[test]
    fn host_authority_must_match_exact_loopback_host_and_bound_port() {
        for host in ["127.0.0.1:8787", "localhost:8787"] {
            let request = Request::builder()
                .header(HOST, host)
                .body(Body::empty())
                .unwrap();
            assert!(host_is_allowed(&request, 8787));
        }
        for host in [
            "127.0.0.1",
            "localhost",
            "127.0.0.1:8788",
            "example.test:8787",
        ] {
            let request = Request::builder()
                .header(HOST, host)
                .body(Body::empty())
                .unwrap();
            assert!(!host_is_allowed(&request, 8787));
        }
        let missing = Request::builder().body(Body::empty()).unwrap();
        assert!(!host_is_allowed(&missing, 8787));
        let empty_origin = Request::builder()
            .header(HOST, "localhost:8787")
            .header(ORIGIN, "")
            .body(Body::empty())
            .unwrap();
        assert!(empty_origin.headers().contains_key(ORIGIN));
    }

    #[tokio::test]
    async fn accepted_time_deadline_covers_a_backpressured_response_write() {
        const RESPONSE_BYTES: usize = 32 * 1024 * 1024;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).await.unwrap();
        let (server_stream, _) = listener.accept().await.unwrap();
        let accepted_at = Instant::now();
        let (_deadline_sender, deadline) = watch::channel(accepted_at + HOOK_REQUEST_DEADLINE);
        let response_ready = Arc::new(Notify::new());
        let handler_ready = Arc::clone(&response_ready);
        let app = Router::new().route(
            "/large",
            get(move || {
                let ready = Arc::clone(&handler_ready);
                async move {
                    let body = Body::from(vec![b'x'; RESPONSE_BYTES]);
                    ready.notify_one();
                    (axum::http::StatusCode::OK, body)
                }
            }),
        );
        let service = TowerToHyperService::new(app);
        let connection = tokio::spawn(serve_connection(server_stream, service, deadline));
        let mut client = client;
        client
            .write_all(
                format!(
                    "GET /large HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                    address.port()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(1), response_ready.notified())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !connection.is_finished(),
            "the handler returned but the large response should be blocked on the unread client"
        );

        timeout(Duration::from_millis(2_300), connection)
            .await
            .expect("connection exceeded its accepted-time hook deadline")
            .unwrap();
        assert!(accepted_at.elapsed() < Duration::from_millis(2_300));
        let mut response = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200"));
        assert!(response.len() < RESPONSE_BYTES);
    }
}
