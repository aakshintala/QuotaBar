use std::env;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use reqwest::Client;
use tokio::runtime::{Builder, Runtime};

use crate::cli::{self, CliError, ClientCommand, Invocation};
use crate::listener::{ListenerError, LoopbackListener};
use crate::providers::http::build_client;
use crate::providers::{claude::ClaudeProbe, codex::CodexProbe, cursor::CursorHttpProbe};
use crate::providers::{muse::MuseHttpProbe, opencode_http::OpenCodeHttpProbe};
use crate::runtime_state::{ControlToken, RuntimeState, RuntimeStateError};
use crate::scheduler::{
    AttemptOutcome, CheckFuture, ProviderCheck, SchedulerClient, SchedulerOwner,
};

const DEFAULT_PORT: u16 = 8787;
const PROVIDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const PROVIDER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The one binary dispatches client commands before entering daemon setup.
pub fn run() -> ExitCode {
    let invocation = match cli::parse_args(env::args_os().skip(1)) {
        Ok(invocation) => invocation,
        Err(CliError::Usage) => {
            eprintln!("quotabard: {}", cli::usage());
            return ExitCode::FAILURE;
        }
        Err(error) => return report_error(error.into()),
    };

    match invocation {
        Invocation::Client(command) => run_client(command),
        Invocation::Daemon => run_daemon(),
    }
}

fn run_client(command: ClientCommand) -> ExitCode {
    let runtime = match current_thread_runtime() {
        Ok(runtime) => runtime,
        Err(error) => return report_error(error),
    };
    match runtime.block_on(cli::run_from_environment(command)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report_error(error.into()),
    }
}

fn run_daemon() -> ExitCode {
    let runtime = match current_thread_runtime() {
        Ok(runtime) => runtime,
        Err(error) => return report_daemon_error(error),
    };
    let home = match explicit_home() {
        Ok(home) => home,
        Err(error) => return report_daemon_error(error),
    };
    let xdg_data_home = env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let xdg_config_home = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let client = match build_client(PROVIDER_REQUEST_TIMEOUT, PROVIDER_CONNECT_TIMEOUT) {
        Ok(client) => client,
        Err(_) => return report_daemon_error(AppError::ProviderSetup),
    };
    let check =
        match production_provider_check(home.clone(), xdg_data_home, xdg_config_home, &client) {
            Ok(check) => check,
            Err(error) => return report_daemon_error(error),
        };

    match runtime.block_on(run_daemon_with(
        &home,
        DEFAULT_PORT,
        check,
        serve_native_listener,
    )) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report_daemon_error(error),
    }
}

fn current_thread_runtime() -> Result<Runtime, AppError> {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| AppError::Runtime)
}

fn explicit_home() -> Result<PathBuf, AppError> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(AppError::Home)?;
    if !home.is_absolute() {
        return Err(AppError::Home);
    }
    Ok(home)
}

fn production_provider_check(
    home: PathBuf,
    xdg_data_home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    client: &Client,
) -> Result<ProviderCheck, AppError> {
    let claude = Arc::new(
        ClaudeProbe::with_client(home.clone(), client).map_err(|_| AppError::ProviderSetup)?,
    );
    let codex = Arc::new(CodexProbe::with_client(home.clone(), client));
    let cursor = Arc::new(
        CursorHttpProbe::with_client(home.clone(), client.clone())
            .map_err(|_| AppError::ProviderSetup)?,
    );
    let opencode = Arc::new(
        OpenCodeHttpProbe::with_client(home.clone(), xdg_data_home, client.clone())
            .map_err(|_| AppError::ProviderSetup)?,
    );
    let muse = Arc::new(
        MuseHttpProbe::with_client(home, xdg_config_home, client)
            .map_err(|_| AppError::ProviderSetup)?,
    );

    Ok(Arc::new(move |id| match id {
        "claude" => {
            let probe = Arc::clone(&claude);
            Box::pin(async move { probe.probe().await }) as CheckFuture
        }
        "codex" => {
            let probe = Arc::clone(&codex);
            Box::pin(async move { probe.probe().await }) as CheckFuture
        }
        "cursor" => {
            let probe = Arc::clone(&cursor);
            Box::pin(async move { probe.probe_at(Utc::now()).await }) as CheckFuture
        }
        "opencode-go" => {
            let probe = Arc::clone(&opencode);
            Box::pin(async move {
                let outcome = probe.probe_at(Utc::now()).await;
                AttemptOutcome {
                    result: outcome.result,
                    retry_after: outcome.retry_after,
                }
            }) as CheckFuture
        }
        "muse" => {
            let probe = Arc::clone(&muse);
            Box::pin(async move { probe.probe().await }) as CheckFuture
        }
        _ => unreachable!("the scheduler owns a fixed five-provider registry"),
    }))
}

async fn run_daemon_with<F, Fut>(
    home: &Path,
    port: u16,
    check: ProviderCheck,
    listen: F,
) -> Result<(), AppError>
where
    F: FnOnce(u16, SchedulerClient, ControlToken) -> Fut,
    Fut: Future<Output = Result<(), AppError>>,
{
    let state = RuntimeState::open(home).map_err(map_state_error)?;
    state.load_config().map_err(|_| AppError::Configuration)?;
    let token = state
        .load_or_create_control_token()
        .map_err(map_state_error)?;
    let (owner, scheduler) = crate::scheduler::start(state, check, Arc::new(Utc::now));
    let result = listen(port, scheduler, token).await;
    shutdown_owner(owner).await;
    result
}

async fn shutdown_owner(owner: SchedulerOwner) {
    owner.shutdown().await;
}

async fn serve_native_listener(
    port: u16,
    scheduler: SchedulerClient,
    token: ControlToken,
) -> Result<(), AppError> {
    let shutdown = native_shutdown_future().map_err(|_| AppError::Signal)?;
    let listener = LoopbackListener::bind(port, scheduler, token)
        .await
        .map_err(map_listener_error)?;
    listener
        .serve(shutdown)
        .await
        .map_err(|_| AppError::Listener)
}

#[cfg(unix)]
fn native_shutdown_future() -> std::io::Result<impl Future<Output = ()> + Send> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
    })
}

#[cfg(not(unix))]
fn native_shutdown_future() -> std::io::Result<impl Future<Output = ()> + Send> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

fn map_state_error(error: RuntimeStateError) -> AppError {
    match error {
        RuntimeStateError::InvalidConfiguration => AppError::Configuration,
        _ => AppError::State,
    }
}

fn map_listener_error(error: ListenerError) -> AppError {
    match error {
        ListenerError::Bind(_) | ListenerError::LocalAddress(_) => AppError::Listener,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppError {
    Runtime,
    Home,
    State,
    Configuration,
    ProviderSetup,
    Listener,
    Signal,
    Client(CliError),
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            _ => formatter.write_str(self.daemon_message().expect("non-client error")),
        }
    }
}

impl AppError {
    fn daemon_message(self) -> Option<&'static str> {
        match self {
            Self::Runtime => Some("could not start the Tokio runtime"),
            Self::Home => Some("HOME must be an absolute path"),
            Self::State => Some("could not open daemon state"),
            Self::Configuration => Some("daemon configuration is invalid"),
            Self::ProviderSetup => Some("could not configure provider checks"),
            Self::Listener => Some("could not bind or serve the loopback listener"),
            Self::Signal => Some("could not wait for daemon shutdown"),
            Self::Client(_) => None,
        }
    }
}

impl Error for AppError {}

impl From<CliError> for AppError {
    fn from(error: CliError) -> Self {
        Self::Client(error)
    }
}

fn report_error(error: AppError) -> ExitCode {
    eprintln!("quotabard: {error}");
    ExitCode::FAILURE
}

fn format_daemon_fatal(message: &str) -> std::ffi::CString {
    std::ffi::CString::new(format!("quotabard: {}", message.replace('\0', "\\0")))
        .expect("NUL bytes are escaped")
}

fn emit_daemon_error(
    error: AppError,
    include_syslog: bool,
    mut stderr: impl FnMut(&str),
    mut syslog: impl FnMut(&std::ffi::CString),
) -> Option<ExitCode> {
    let message = error.daemon_message()?;
    let diagnostic = format!("quotabard: {message}");
    let record = format_daemon_fatal(message);
    stderr(&diagnostic);
    if include_syslog {
        syslog(&record);
    }
    Some(ExitCode::FAILURE)
}

fn report_daemon_error(error: AppError) -> ExitCode {
    emit_daemon_error(
        error,
        cfg!(target_os = "macos"),
        |record| eprintln!("{record}"),
        |record| {
            #[cfg(target_os = "macos")]
            log_daemon_fatal(record);
            #[cfg(not(target_os = "macos"))]
            let _ = record;
        },
    )
    .unwrap_or_else(|| report_error(error))
}

#[cfg(target_os = "macos")]
fn log_daemon_fatal(record: &std::ffi::CString) {
    const SYSLOG_FORMAT: &[u8] = b"%s\0";
    // SAFETY: SYSLOG_FORMAT is a static NUL-terminated string. record remains
    // alive for the call, its interior NULs are escaped, and the fixed format
    // treats record as data rather than as a format string.
    unsafe {
        libc::syslog(
            libc::LOG_ERR,
            SYSLOG_FORMAT.as_ptr().cast(),
            record.as_ptr(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppError, emit_daemon_error, format_daemon_fatal, production_provider_check,
        run_daemon_with,
    };
    use crate::cli::CliError;
    use crate::feed::ErrorCode;
    use crate::listener::LoopbackListener;
    use crate::providers::http::build_client;
    use crate::runtime_state::{RuntimeState, RuntimeStateError};
    use crate::scheduler::{AttemptOutcome, CheckFuture, ProviderCheck, SchedulerClient};
    use reqwest::header::HOST;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::{Notify, oneshot};

    static NEXT_HOME: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn daemon_fatal_format_has_a_fixed_prefix_and_static_message() {
        let record = format_daemon_fatal("daemon configuration is invalid");
        assert_eq!(
            record.to_bytes(),
            b"quotabard: daemon configuration is invalid"
        );
    }

    #[test]
    fn daemon_fatal_format_escapes_interior_nul_bytes() {
        let record = format_daemon_fatal("invalid\0message");
        assert_eq!(record.to_bytes(), b"quotabard: invalid\\0message");
    }

    #[test]
    fn daemon_fatal_emission_keeps_stderr_and_adds_syslog_on_macos() {
        let mut stderr = Vec::new();
        let mut syslog = Vec::new();
        let status = emit_daemon_error(
            AppError::Configuration,
            true,
            |record| stderr.push(record.to_owned()),
            |record| syslog.push(record.to_bytes().to_vec()),
        );
        assert!(status.is_some());
        assert_eq!(stderr, ["quotabard: daemon configuration is invalid"]);
        assert_eq!(
            syslog,
            [b"quotabard: daemon configuration is invalid".to_vec()]
        );
    }

    #[test]
    fn daemon_fatal_emission_is_stderr_only_on_linux() {
        let mut stderr = Vec::new();
        let mut syslog = Vec::new();
        let status = emit_daemon_error(
            AppError::Configuration,
            false,
            |record| stderr.push(record.to_owned()),
            |record| syslog.push(record.to_bytes().to_vec()),
        );
        assert!(status.is_some());
        assert_eq!(stderr, ["quotabard: daemon configuration is invalid"]);
        assert!(syslog.is_empty());
    }

    #[test]
    fn client_errors_use_neither_daemon_fatal_sink() {
        let mut stderr = Vec::new();
        let mut syslog = Vec::new();
        let status = emit_daemon_error(
            AppError::Client(CliError::DaemonUnavailable),
            true,
            |record| stderr.push(record.to_owned()),
            |record| syslog.push(record.to_bytes().to_vec()),
        );
        assert!(status.is_none());
        assert!(stderr.is_empty());
        assert!(syslog.is_empty());
    }

    struct Home(PathBuf);

    impl Home {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "quotabard-app-{}-{}",
                std::process::id(),
                NEXT_HOME.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fake_check(ids: Arc<Mutex<Vec<&'static str>>>, started: Arc<Notify>) -> ProviderCheck {
        Arc::new(move |id| {
            let mut ids = ids.lock().unwrap();
            ids.push(id);
            if ids.len() == 5 {
                started.notify_one();
            }
            Box::pin(async { AttemptOutcome::failure(ErrorCode::CredentialsMissing) })
                as CheckFuture
        })
    }

    #[test]
    fn all_five_real_probe_adapters_construct_without_credential_or_network_io() {
        let home = Home::new();
        let client = build_client(Duration::from_secs(15), Duration::from_secs(5)).unwrap();
        let checks =
            production_provider_check(home.path().to_path_buf(), None, None, &client).unwrap();
        drop(checks);
        assert!(!home.path().join(".quotabar").exists());
        assert!(!home.path().join(".claude").exists());
        assert!(!home.path().join(".codex").exists());
        assert!(!home.path().join(".config").exists());
    }

    #[tokio::test]
    async fn loopback_get_uses_fake_checks_for_all_five_scheduler_slots_then_drains_on_shutdown() {
        let home = Home::new();
        let ids = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let check = fake_check(Arc::clone(&ids), Arc::clone(&started));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let listen = move |port: u16, scheduler: SchedulerClient, token| async move {
            assert_eq!(port, 0);
            let listener = LoopbackListener::bind(0, scheduler, token)
                .await
                .map_err(|_| AppError::Listener)?;
            let address = listener.local_addr();
            let shutdown = async move {
                let _ = shutdown_rx.await;
            };
            let server = tokio::spawn(listener.serve(shutdown));
            let response = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(8))
                .build()
                .unwrap()
                .get(format!("http://127.0.0.1:{}/quotas", address.port()))
                .header(HOST, format!("127.0.0.1:{}", address.port()))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            let feed: crate::feed::FeedDto = response.json().await.unwrap();
            assert_eq!(feed.providers.len(), 5);
            started.notified().await;
            let mut seen = ids.lock().unwrap().clone();
            seen.sort_unstable();
            assert_eq!(seen, ["claude", "codex", "cursor", "muse", "opencode-go"]);
            let _ = shutdown_tx.send(());
            server.await.unwrap().unwrap();
            Ok(())
        };
        run_daemon_with(home.path(), 0, check, listen)
            .await
            .unwrap();
        drop(RuntimeState::open(home.path()).unwrap());
    }

    #[tokio::test]
    async fn normal_shutdown_awaits_inflight_worker_drain_before_releasing_instance_lock() {
        let home = Home::new();
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let lock_was_held = Arc::new(AtomicUsize::new(0));
        let check = {
            let started = Arc::clone(&started);
            let dropped = Arc::clone(&dropped);
            let lock_was_held = Arc::clone(&lock_was_held);
            let path = home.path().to_path_buf();
            Arc::new(move |_id| {
                let worker = WorkerDrop {
                    home: path.clone(),
                    dropped: Arc::clone(&dropped),
                    lock_was_held: Arc::clone(&lock_was_held),
                };
                let started = Arc::clone(&started);
                Box::pin(async move {
                    let _worker = worker;
                    started.notify_one();
                    std::future::pending::<AttemptOutcome>().await
                }) as CheckFuture
            }) as ProviderCheck
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let listen = move |_port: u16, scheduler: SchedulerClient, token| async move {
            let listener = LoopbackListener::bind(0, scheduler.clone(), token)
                .await
                .map_err(|_| AppError::Listener)?;
            let _ = scheduler.refresh(Duration::ZERO).await;
            started.notified().await;
            let server = tokio::spawn(listener.serve(async move {
                let _ = shutdown_rx.await;
            }));
            let _ = shutdown_tx.send(());
            server.await.unwrap().map_err(|_| AppError::Listener)?;
            Ok(())
        };
        run_daemon_with(home.path(), 0, check, listen)
            .await
            .unwrap();
        dropped.notified().await;
        assert_eq!(lock_was_held.load(Ordering::SeqCst), 1);
        drop(RuntimeState::open(home.path()).unwrap());
    }

    struct WorkerDrop {
        home: PathBuf,
        dropped: Arc<Notify>,
        lock_was_held: Arc<AtomicUsize>,
    }

    impl Drop for WorkerDrop {
        fn drop(&mut self) {
            let held = matches!(
                RuntimeState::open(&self.home),
                Err(RuntimeStateError::AlreadyRunning)
            );
            self.lock_was_held.store(held as usize, Ordering::SeqCst);
            self.dropped.notify_one();
        }
    }

    #[tokio::test]
    async fn listener_failure_awaits_worker_drain_before_releasing_instance_lock() {
        let home = Home::new();
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let lock_was_held = Arc::new(AtomicUsize::new(0));
        let check = {
            let started = Arc::clone(&started);
            let dropped = Arc::clone(&dropped);
            let lock_was_held = Arc::clone(&lock_was_held);
            let path = home.path().to_path_buf();
            Arc::new(move |_id| {
                let worker = WorkerDrop {
                    home: path.clone(),
                    dropped: Arc::clone(&dropped),
                    lock_was_held: Arc::clone(&lock_was_held),
                };
                let started = Arc::clone(&started);
                Box::pin(async move {
                    let _worker = worker;
                    started.notify_one();
                    std::future::pending::<AttemptOutcome>().await
                }) as CheckFuture
            }) as ProviderCheck
        };
        let listen = |_port: u16, scheduler: SchedulerClient, _token| async move {
            let _ = scheduler.refresh(Duration::ZERO).await;
            started.notified().await;
            Err(AppError::Listener)
        };
        assert_eq!(
            run_daemon_with(home.path(), 0, check, listen).await,
            Err(AppError::Listener)
        );
        dropped.notified().await;
        assert_eq!(lock_was_held.load(Ordering::SeqCst), 1);
        drop(RuntimeState::open(home.path()).unwrap());
    }

    #[tokio::test]
    async fn malformed_config_fails_before_listener_token_creation_or_provider_work() {
        let home = Home::new();
        fs::create_dir(home.path().join(".quotabar")).unwrap();
        fs::write(home.path().join(".quotabar/daemon.json"), b"{malformed").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let check: ProviderCheck = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { AttemptOutcome::failure(ErrorCode::ProviderError) }) as CheckFuture
            })
        };
        let listener_called = Arc::new(AtomicUsize::new(0));
        let listen = {
            let listener_called = Arc::clone(&listener_called);
            move |_port: u16, _scheduler: SchedulerClient, _token| async move {
                listener_called.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };
        assert_eq!(
            run_daemon_with(home.path(), 0, check, listen).await,
            Err(AppError::Configuration)
        );
        assert_eq!(listener_called.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!home.path().join(".quotabar/daemon/control-token").exists());
        drop(RuntimeState::open(home.path()).unwrap());
    }

    #[tokio::test]
    async fn injected_signal_setup_or_bind_failure_still_releases_state_lock() {
        let home = Home::new();
        let check: ProviderCheck = Arc::new(|_| {
            Box::pin(async { AttemptOutcome::failure(ErrorCode::ProviderError) }) as CheckFuture
        });
        let listen = |_port: u16, _scheduler: SchedulerClient, _token| async move {
            // This is the same cleanup path used for signal registration or bind failure.
            Err(AppError::Signal)
        };
        assert_eq!(
            run_daemon_with(home.path(), 0, check, listen).await,
            Err(AppError::Signal)
        );
        drop(RuntimeState::open(home.path()).unwrap());
    }
}
