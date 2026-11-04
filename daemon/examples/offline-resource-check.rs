use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use quotabard::feed::ErrorCode;
use quotabard::listener::LoopbackListener;
use quotabard::quota::{Observation, Quota};
use quotabard::runtime_state::RuntimeState;
use quotabard::scheduler::{self, AttemptOutcome, ProviderCheck, WallNow};
use reqwest::Client;
use reqwest::redirect::Policy;
use tokio::sync::oneshot;
use tokio::time::sleep;

const PROVIDERS: [&str; 5] = ["claude", "codex", "cursor", "opencode-go", "muse"];
const CHECK_DELAY: Duration = Duration::from_millis(50);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FEED_BYTES: usize = 64 * 1024;
const CACHE_REQUESTS: usize = 1_000;
const DEFAULT_IDLE_SECONDS: u64 = 60;
const MAX_IDLE_SECONDS: u64 = 300;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let idle_seconds = parse_args()?;
    let http = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let home = TemporaryHome::create()?;
    let state = RuntimeState::open(home.path())?;
    let token = state.load_or_create_control_token()?;
    let checks = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let check = synthetic_check(
        Arc::clone(&checks),
        Arc::clone(&active),
        Arc::clone(&max_active),
    );
    let wall_now: WallNow = Arc::new(Utc::now);
    let (owner, client) = scheduler::start(state, check, wall_now);
    let listener = match LoopbackListener::bind(0, client, token).await {
        Ok(listener) => listener,
        Err(error) => {
            owner.shutdown().await;
            return Err(error.into());
        }
    };
    let address = listener.local_addr();
    let (stop_listener, stop) = oneshot::channel();
    let listener_task = tokio::spawn(listener.serve(async move {
        let _ = stop.await;
    }));
    let url = format!("http://{address}/quotas");

    let result = async {
        stage("idle-before-request", address.port(), 0, idle_seconds);
    sleep(Duration::from_secs(idle_seconds)).await;
    let idle_checks = checks.load(Ordering::SeqCst);
    if idle_checks != 0 {
        return Err(format!("provider checks ran during quiet idle: {idle_checks}").into());
    }

    stage("first-refresh", address.port(), idle_checks, 1);
    let response = get_bounded(&http, &url).await?;
    if !response.status.is_success() || !valid_feed(&response.body) {
        return Err("first HTTP response was not a valid quota feed".into());
    }
    let after_refresh = checks.load(Ordering::SeqCst);
    if after_refresh != PROVIDERS.len() {
        return Err(
            format!("expected five checks after first refresh, got {after_refresh}").into(),
        );
    }
    if max_active.load(Ordering::SeqCst) != PROVIDERS.len() {
        return Err(format!(
            "expected five concurrent synthetic checks, observed {}",
            max_active.load(Ordering::SeqCst)
        )
        .into());
    }

    stage(
        "cached-http",
        address.port(),
        after_refresh,
        CACHE_REQUESTS as u64,
    );
    let mut cache_elapsed = Duration::ZERO;
    for _ in 0..CACHE_REQUESTS {
        let started = Instant::now();
        let response = get_bounded(&http, &url).await?;
        cache_elapsed += started.elapsed();
        if !response.status.is_success() || !valid_feed(&response.body) {
            return Err("cached HTTP response was not a valid quota feed".into());
        }
    }
    let after_cached_gets = checks.load(Ordering::SeqCst);
    if after_cached_gets != after_refresh {
        return Err(format!("cached GETs triggered extra checks: {after_cached_gets}").into());
    }

    stage(
        "idle-after-activity",
        address.port(),
        after_cached_gets,
        idle_seconds,
    );
    sleep(Duration::from_secs(idle_seconds)).await;
    if checks.load(Ordering::SeqCst) != after_cached_gets {
        return Err("provider checks recurred during post-activity idle".into());
    }
        println!(
            "result=pass providers={} max_concurrent_checks={} cached_gets={} cached_get_total_ms={:.3}",
            PROVIDERS.len(),
            max_active.load(Ordering::SeqCst),
            CACHE_REQUESTS,
            cache_elapsed.as_secs_f64() * 1_000.0
        );
        Ok::<(), Box<dyn Error>>(())
    }
    .await;

    let _ = stop_listener.send(());
    let listener_result = listener_task.await;
    owner.shutdown().await;
    drop(home);
    result?;
    listener_result??;
    Ok(())
}

fn parse_args() -> Result<u64, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--run-offline-resource-check") {
        return Err(
            "no work performed; pass --run-offline-resource-check [--idle-seconds 30..300]".into(),
        );
    }
    let mut idle_seconds = DEFAULT_IDLE_SECONDS;
    if let Some(option) = args.next() {
        if option != "--idle-seconds" {
            return Err("expected --idle-seconds".into());
        }
        let value = args
            .next()
            .ok_or("missing idle-seconds value")?
            .parse::<u64>()?;
        if !(30..=MAX_IDLE_SECONDS).contains(&value) {
            return Err(format!("idle-seconds must be in 30..={MAX_IDLE_SECONDS}").into());
        }
        idle_seconds = value;
    }
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    Ok(idle_seconds)
}

fn synthetic_check(
    checks: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
) -> ProviderCheck {
    Arc::new(move |id| {
        let checks = Arc::clone(&checks);
        let active = Arc::clone(&active);
        let max_active = Arc::clone(&max_active);
        Box::pin(async move {
            checks.fetch_add(1, Ordering::SeqCst);
            let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
            max_active.fetch_max(now_active, Ordering::SeqCst);
            sleep(CHECK_DELAY).await;
            active.fetch_sub(1, Ordering::SeqCst);
            if !PROVIDERS.contains(&id) {
                return AttemptOutcome::failure(ErrorCode::InternalError);
            }
            AttemptOutcome::success(Observation {
                captured_at: Utc::now(),
                tier: Some("synthetic".to_owned()),
                quotas: vec![Quota {
                    key: "offline-example".to_owned(),
                    label: "Synthetic offline observation".to_owned(),
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
            })
        })
    })
}

struct BoundedResponse {
    status: reqwest::StatusCode,
    body: Vec<u8>,
}

async fn get_bounded(client: &Client, url: &str) -> Result<BoundedResponse, Box<dyn Error>> {
    let response = client.get(url).send().await?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_FEED_BYTES as u64)
    {
        return Err("response exceeded the 64 KiB example bound".into());
    }
    let status = response.status();
    let body = response.bytes().await?;
    if body.len() > MAX_FEED_BYTES {
        return Err("response exceeded the 64 KiB example bound".into());
    }
    Ok(BoundedResponse {
        status,
        body: body.to_vec(),
    })
}

fn valid_feed(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("providers")
                .and_then(serde_json::Value::as_array)
                .map(|providers| providers.len())
        })
        == Some(PROVIDERS.len())
}

fn stage(name: &str, port: u16, checks: usize, count: u64) {
    println!(
        "stage={name} pid={} port={port} checks={checks} count={count}",
        std::process::id()
    );
}

struct TemporaryHome(PathBuf);

impl TemporaryHome {
    fn create() -> io::Result<Self> {
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let name = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let path = base.join(format!("quotabar-offline-resource-{name}"));
            #[cfg(unix)]
            let created = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700).create(&path)
            };
            #[cfg(not(unix))]
            let created = fs::create_dir(&path);
            match created {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate unique temporary HOME",
        ))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
