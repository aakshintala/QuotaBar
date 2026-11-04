use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{Id, JoinSet};
use tokio::time::{Instant, timeout, timeout_at};

use crate::feed::{ErrorCode, FeedDto, ProviderState, project_feed};
use crate::quota::Observation;
use crate::runtime_state::{DaemonConfig, RuntimeState, RuntimeStateError};

const PROVIDERS: [(&str, &str); 5] = [
    ("claude", "Claude"),
    ("codex", "Codex"),
    ("cursor", "Cursor"),
    ("opencode-go", "OpenCode Go"),
    ("muse", "Muse"),
];
const COMMAND_CAPACITY: usize = 32;
const MAX_ATTEMPT: Duration = Duration::from_secs(30);
const MAX_REFRESH_WAIT: Duration = Duration::from_millis(4_500);
const REFRESH_DEADLINE: Duration = Duration::from_secs(5);
const CACHED_DEADLINE: Duration = Duration::from_secs(2);
const REUSE_FOR: ChronoDuration = ChronoDuration::seconds(300);
const MUSE_GUARD: ChronoDuration = ChronoDuration::seconds(300);

pub type CheckFuture = Pin<Box<dyn Future<Output = AttemptOutcome> + Send>>;
pub type ProviderCheck = Arc<dyn Fn(&'static str) -> CheckFuture + Send + Sync>;
pub type WallNow = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Result from a provider attempt. Capture time is assigned by the scheduler
/// when a successful result completes, not by the provider callback.
#[derive(Clone, Debug, PartialEq)]
pub struct AttemptOutcome {
    pub result: Result<Observation, ErrorCode>,
    pub retry_after: Option<DateTime<Utc>>,
}

impl AttemptOutcome {
    pub fn success(observation: Observation) -> Self {
        Self {
            result: Ok(observation),
            retry_after: None,
        }
    }

    pub fn failure(error: ErrorCode) -> Self {
        Self {
            result: Err(error),
            retry_after: None,
        }
    }
}

/// Service-level failures are deliberately separate from provider ErrorCode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestError {
    Busy,
    Unavailable,
    Configuration,
    InvalidProvider,
    Persistence,
    Deadline,
}

#[derive(Clone)]
pub struct SchedulerClient {
    commands: mpsc::Sender<Command>,
}

pub struct SchedulerOwner {
    shutdown_signal: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SchedulerOwner {
    /// Stop provider futures, wait for their cancellation, then release the
    /// runtime-state lock when the actor task returns.
    pub async fn shutdown(mut self) {
        if let Some(signal) = self.shutdown_signal.take() {
            let _ = signal.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SchedulerOwner {
    fn drop(&mut self) {
        if let Some(signal) = self.shutdown_signal.take() {
            let _ = signal.send(());
        }
    }
}

impl SchedulerClient {
    /// Trigger or join eligible on-demand checks. Caller cancellation only
    /// drops its completion receivers; provider work belongs to the owner.
    pub async fn refresh(&self, wait: Duration) -> Result<FeedDto, RequestError> {
        let started = Instant::now();
        let deadline = started + REFRESH_DEADLINE;
        let (reply, response) = oneshot::channel();
        self.admit(Command::Refresh(reply))?;
        let mut completions = timeout_at(deadline, response)
            .await
            .map_err(|_| RequestError::Deadline)?
            .map_err(|_| RequestError::Unavailable)??;

        let wait_deadline = (started + wait.min(MAX_REFRESH_WAIT)).min(deadline);
        for completion in &mut completions {
            if !*completion.borrow() {
                let _ = timeout_at(wait_deadline, completion.wait_for(|done| *done)).await;
            }
        }
        self.read_feed_until(deadline).await
    }

    /// Persist and apply a provider toggle in the scheduler actor. Configuration
    /// commits are applied even when the final directory sync is unconfirmed.
    pub async fn set_enabled(&self, id: String, enabled: bool) -> Result<(), RequestError> {
        let (reply, response) = oneshot::channel();
        self.admit(Command::SetEnabled { id, enabled, reply })?;
        timeout(REFRESH_DEADLINE, response)
            .await
            .map_err(|_| RequestError::Deadline)?
            .map_err(|_| RequestError::Unavailable)?
    }

    /// Return the current cache without starting or joining provider work.
    pub async fn cached(&self) -> Result<FeedDto, RequestError> {
        self.read_feed_until(Instant::now() + CACHED_DEADLINE).await
    }

    async fn read_feed_until(&self, deadline: Instant) -> Result<FeedDto, RequestError> {
        let (reply, response) = oneshot::channel();
        self.admit(Command::Feed(reply))?;
        timeout_at(deadline, response)
            .await
            .map_err(|_| RequestError::Deadline)?
            .map_err(|_| RequestError::Unavailable)?
    }

    fn admit(&self, command: Command) -> Result<(), RequestError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => RequestError::Busy,
                mpsc::error::TrySendError::Closed(_) => RequestError::Unavailable,
            })
    }
}

/// Start one actor. Constructing it and leaving it idle never invokes `check`.
pub fn start(
    state: RuntimeState,
    check: ProviderCheck,
    wall_now: WallNow,
) -> (SchedulerOwner, SchedulerClient) {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    let actor = Actor::new(state, check, wall_now);
    let (shutdown_signal, shutdown) = oneshot::channel();
    let task = tokio::spawn(actor.run(receiver, shutdown));
    (
        SchedulerOwner {
            shutdown_signal: Some(shutdown_signal),
            task: Some(task),
        },
        SchedulerClient { commands },
    )
}

#[derive(Default)]
struct ProviderCache {
    observation: Option<Observation>,
    error: Option<ErrorCode>,
    generation: u64,
    in_flight: Option<watch::Sender<bool>>,
    cooldown_loaded: bool,
    cooldown: Option<DateTime<Utc>>,
    pending_persist: Option<DateTime<Utc>>,
}

enum Command {
    Refresh(oneshot::Sender<Result<Vec<watch::Receiver<bool>>, RequestError>>),
    Feed(oneshot::Sender<Result<FeedDto, RequestError>>),
    SetEnabled {
        id: String,
        enabled: bool,
        reply: oneshot::Sender<Result<(), RequestError>>,
    },
}

struct Actor {
    state: RuntimeState,
    check: ProviderCheck,
    wall_now: WallNow,
    disabled: Vec<String>,
    providers: [ProviderCache; PROVIDERS.len()],
    workers: JoinSet<(usize, u64, AttemptOutcome)>,
    worker_ids: HashMap<Id, (usize, u64)>,
}

impl Actor {
    fn new(state: RuntimeState, check: ProviderCheck, wall_now: WallNow) -> Self {
        Self {
            state,
            check,
            wall_now,
            disabled: Vec::new(),
            providers: std::array::from_fn(|_| ProviderCache::default()),
            workers: JoinSet::new(),
            worker_ids: HashMap::new(),
        }
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut shutdown: oneshot::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break,
                command = commands.recv() => match command {
                    Some(Command::Refresh(reply)) => {
                        let result = self.begin_refresh();
                        let _ = reply.send(result);
                    }
                    Some(Command::Feed(reply)) => {
                        let result = self.feed();
                        let _ = reply.send(result);
                    }
                    Some(Command::SetEnabled { id, enabled, reply }) => {
                        if !reply.is_closed() {
                            let result = self.set_enabled(&id, enabled);
                            let _ = reply.send(result);
                        }
                    }
                    None => break,
                },
                completed = self.workers.join_next_with_id(), if !self.workers.is_empty() => {
                    if let Some(completed) = completed {
                        self.complete(completed);
                    }
                }
            }
        }
        self.workers.abort_all();
        while self.workers.join_next().await.is_some() {}
        self.worker_ids.clear();
    }

    fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), RequestError> {
        let update = self
            .state
            .set_enabled(id, enabled)
            .map_err(|error| match error {
                RuntimeStateError::InvalidProvider => RequestError::InvalidProvider,
                RuntimeStateError::Persistence(_) => RequestError::Persistence,
                _ => RequestError::Configuration,
            })?;
        // The rename is the commit point. Apply that value even if directory
        // durability was not confirmed, so disabled results are hidden now.
        self.apply_configuration(update.config);
        Ok(())
    }

    fn begin_refresh(&mut self) -> Result<Vec<watch::Receiver<bool>>, RequestError> {
        self.reload_configuration()?;
        let now = (self.wall_now)();
        self.discard_future_observations(&now);
        let mut completions = Vec::with_capacity(PROVIDERS.len());

        for (index, (provider_id, _)) in PROVIDERS.iter().enumerate() {
            if self.disabled.iter().any(|id| id == provider_id) {
                continue;
            }
            if let Some(active) = &self.providers[index].in_flight {
                completions.push(active.subscribe());
                continue;
            }
            let is_stale = self.providers[index]
                .observation
                .as_ref()
                .is_none_or(|observation| {
                    now.signed_duration_since(observation.captured_at) >= REUSE_FOR
                });
            if !is_stale {
                continue;
            }
            if !self.prepare_provider(index, now) {
                continue;
            }

            let generation = self.providers[index].generation;
            let (completion, waiter) = watch::channel(false);
            self.providers[index].in_flight = Some(completion);
            let provider_id = PROVIDERS[index].0;
            let check = Arc::clone(&self.check);
            let task = self.workers.spawn(async move {
                let outcome = match timeout(MAX_ATTEMPT, check(provider_id)).await {
                    Ok(outcome) => outcome,
                    Err(_) => AttemptOutcome::failure(ErrorCode::NetworkError),
                };
                (index, generation, outcome)
            });
            self.worker_ids.insert(task.id(), (index, generation));
            completions.push(waiter);
        }
        Ok(completions)
    }

    fn prepare_provider(&mut self, index: usize, now: DateTime<Utc>) -> bool {
        let id = PROVIDERS[index].0;
        if let Some(deadline) = self.providers[index].pending_persist {
            match self.state.extend_cooldown(id, deadline, now) {
                Ok(saved) => {
                    self.providers[index].pending_persist = None;
                    self.providers[index].cooldown = Some(saved);
                    if saved > now {
                        self.providers[index].error = Some(ErrorCode::Cooldown);
                        return false;
                    }
                }
                Err(_) => {
                    self.providers[index].error = Some(ErrorCode::PersistenceError);
                    return false;
                }
            }
        }

        if !self.providers[index].cooldown_loaded {
            match self.state.load_cooldown(id, now) {
                Ok(saved) => {
                    self.providers[index].cooldown_loaded = true;
                    self.providers[index].cooldown = saved.not_before;
                }
                Err(_) => {
                    self.providers[index].error = Some(ErrorCode::PersistenceError);
                    return false;
                }
            }
        }
        if self.providers[index]
            .cooldown
            .is_some_and(|deadline| deadline > now)
        {
            self.providers[index].error = Some(ErrorCode::Cooldown);
            return false;
        }

        if id == "muse" {
            let Some(deadline) = now.checked_add_signed(MUSE_GUARD) else {
                self.providers[index].error = Some(ErrorCode::PersistenceError);
                return false;
            };
            match self.state.extend_cooldown(id, deadline, now) {
                Ok(saved) => self.providers[index].cooldown = Some(saved),
                Err(_) => {
                    self.providers[index].pending_persist = Some(deadline);
                    self.providers[index].cooldown = Some(deadline);
                    self.providers[index].error = Some(ErrorCode::PersistenceError);
                    return false;
                }
            }
        }
        true
    }

    fn feed(&mut self) -> Result<FeedDto, RequestError> {
        self.reload_configuration()?;
        let now = (self.wall_now)();
        self.discard_future_observations(&now);
        let states: Vec<_> = self
            .providers
            .iter()
            .enumerate()
            .map(|(index, provider)| ProviderState {
                id: PROVIDERS[index].0,
                name: PROVIDERS[index].1,
                observation: provider.observation.as_ref(),
                error_code: provider.error,
                busy: provider.in_flight.is_some(),
            })
            .collect();
        Ok(project_feed(&now, &states, &self.disabled))
    }

    fn reload_configuration(&mut self) -> Result<(), RequestError> {
        let config = self
            .state
            .load_config()
            .map_err(|_| RequestError::Configuration)?;
        self.apply_configuration(config);
        Ok(())
    }

    fn apply_configuration(&mut self, config: DaemonConfig) {
        for (index, (id, _)) in PROVIDERS.iter().enumerate() {
            let was_disabled = self.disabled.iter().any(|disabled| disabled == id);
            let is_disabled = config
                .disabled_providers
                .iter()
                .any(|disabled| disabled == id);
            if was_disabled != is_disabled {
                self.providers[index].generation = self.providers[index].generation.wrapping_add(1);
                if is_disabled {
                    self.providers[index].observation = None;
                    self.providers[index].error = None;
                }
            }
        }
        self.disabled = config.disabled_providers;
    }

    fn discard_future_observations(&mut self, now: &DateTime<Utc>) {
        for provider in &mut self.providers {
            if provider
                .observation
                .as_ref()
                .is_some_and(|observation| &observation.captured_at > now)
            {
                provider.observation = None;
            }
        }
    }

    fn complete(
        &mut self,
        completed: Result<(Id, (usize, u64, AttemptOutcome)), tokio::task::JoinError>,
    ) {
        let (task_id, result) = match completed {
            Ok((id, result)) => (id, Some(result)),
            Err(error) => (error.id(), None),
        };
        let Some((index, generation)) = self.worker_ids.remove(&task_id) else {
            return;
        };
        let now = (self.wall_now)();
        let mut persist_failed = false;
        let outcome = result.map(|(_, _, outcome)| outcome);
        if let Some(outcome) = &outcome
            && let Some(deadline) = self.retry_deadline(index, outcome, &now)
        {
            match self
                .state
                .extend_cooldown(PROVIDERS[index].0, deadline, now)
            {
                Ok(saved) => self.providers[index].cooldown = Some(saved),
                Err(_) => {
                    persist_failed = true;
                    self.providers[index].pending_persist = Some(
                        self.providers[index]
                            .cooldown
                            .filter(|known| *known > deadline)
                            .unwrap_or(deadline),
                    );
                }
            }
        }

        let is_current = self.providers[index].generation == generation
            && !self.disabled.iter().any(|id| id == PROVIDERS[index].0);
        if is_current {
            match outcome {
                Some(AttemptOutcome {
                    result: Ok(mut observation),
                    ..
                }) => {
                    observation.captured_at = now;
                    self.providers[index].observation = Some(observation);
                    self.providers[index].error =
                        persist_failed.then_some(ErrorCode::PersistenceError);
                }
                Some(AttemptOutcome {
                    result: Err(error), ..
                }) => {
                    self.providers[index].error = Some(if persist_failed {
                        ErrorCode::PersistenceError
                    } else {
                        error
                    });
                }
                None => self.providers[index].error = Some(ErrorCode::InternalError),
            }
        }
        if let Some(completion) = self.providers[index].in_flight.take() {
            completion.send_replace(true);
        }
    }

    fn retry_deadline(
        &self,
        index: usize,
        outcome: &AttemptOutcome,
        now: &DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let valid_retry_after = outcome.retry_after.filter(|value| value > now);
        if valid_retry_after.is_some() {
            return valid_retry_after;
        }
        if PROVIDERS[index].0 == "claude" && outcome.result == Err(ErrorCode::RateLimited) {
            return now.checked_add_signed(ChronoDuration::seconds(300));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AttemptOutcome, Command, MAX_ATTEMPT, ProviderCheck, RequestError, SchedulerClient,
        WallNow, start,
    };
    use crate::feed::ErrorCode;
    use crate::quota::{Observation, Quota};
    use crate::runtime_state::{RuntimeState, RuntimeStateError};
    use chrono::{DateTime, Utc};
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::{Barrier, Notify, oneshot, watch};

    fn wall(seconds: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(seconds, 0).unwrap()
    }

    fn observation(captured: DateTime<Utc>) -> Observation {
        Observation {
            captured_at: captured,
            tier: Some("Pro".into()),
            quotas: vec![Quota {
                key: "session".into(),
                label: "Session".into(),
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

    struct Home(std::path::PathBuf);

    impl Home {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "quotabard-scheduler-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn wall_clock(value: Arc<AtomicI64>) -> WallNow {
        Arc::new(move || wall(value.load(Ordering::SeqCst)))
    }

    fn check_with<F, Fut>(callback: F) -> ProviderCheck
    where
        F: Fn(&'static str) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = AttemptOutcome> + Send + 'static,
    {
        Arc::new(move |id| {
            Box::pin(callback(id)) as Pin<Box<dyn Future<Output = AttemptOutcome> + Send>>
        })
    }

    fn check_success(counter: Arc<AtomicUsize>) -> ProviderCheck {
        check_with(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { AttemptOutcome::success(observation(wall(1_000))) }
        })
    }

    async fn rig(
        check: ProviderCheck,
        now: i64,
    ) -> (Home, super::SchedulerOwner, SchedulerClient, Arc<AtomicI64>) {
        let home = Home::new();
        let state = RuntimeState::open(&home.0).unwrap();
        let clock = Arc::new(AtomicI64::new(now));
        let (owner, client) = start(state, check, wall_clock(Arc::clone(&clock)));
        (home, owner, client, clock)
    }

    fn feed_provider<'a>(feed: &'a crate::feed::FeedDto, id: &str) -> &'a crate::feed::ProviderDto {
        feed.providers
            .iter()
            .find(|provider| provider.id == id)
            .unwrap()
    }

    fn config_path(home: &Home) -> std::path::PathBuf {
        home.0.join(".quotabar/daemon.json")
    }

    fn cooldown_path(home: &Home, id: &str) -> std::path::PathBuf {
        home.0.join(format!(".quotabar/daemon/{id}.json"))
    }

    fn write_config(home: &Home, bytes: &[u8]) {
        std::fs::write(config_path(home), bytes).unwrap();
    }

    async fn begin(client: &SchedulerClient) -> Vec<watch::Receiver<bool>> {
        let (reply, response) = oneshot::channel();
        client.admit(Command::Refresh(reply)).unwrap();
        response.await.unwrap().unwrap()
    }

    async fn wait_for_all(mut completions: Vec<watch::Receiver<bool>>) {
        for completion in &mut completions {
            if !*completion.borrow() {
                completion.wait_for(|done| *done).await.unwrap();
            }
        }
    }

    fn cooldown_deadline(home: &Home, id: &str) -> DateTime<Utc> {
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(cooldown_path(home, id)).unwrap()).unwrap();
        DateTime::parse_from_rfc3339(record["notBefore"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc)
    }

    #[tokio::test(start_paused = true)]
    async fn idle_and_cached_reads_never_call_a_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_home, owner, client, _) = rig(check_success(Arc::clone(&calls)), 1_000).await;
        let cached = client.cached().await.unwrap();
        assert_eq!(cached.providers.len(), 5);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn command_admission_fails_immediately_when_bounded_channel_is_full() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_home, owner, client, _) = rig(check_success(calls), 1_000).await;
        let mut replies = Vec::new();
        for _ in 0..super::COMMAND_CAPACITY {
            let (reply, response) = oneshot::channel();
            client.admit(Command::Feed(reply)).unwrap();
            replies.push(response);
        }
        assert_eq!(client.cached().await.unwrap_err(), RequestError::Busy);
        drop(replies);
        owner.shutdown().await;
    }

    struct LockProbe {
        home: PathBuf,
        outcome: Arc<AtomicUsize>,
        dropped: Arc<Notify>,
    }

    impl Drop for LockProbe {
        fn drop(&mut self) {
            let outcome = match RuntimeState::open(&self.home) {
                Err(RuntimeStateError::AlreadyRunning) => 1,
                Ok(state) => {
                    drop(state);
                    2
                }
                Err(_) => 3,
            };
            self.outcome.store(outcome, Ordering::SeqCst);
            self.dropped.notify_one();
        }
    }

    #[tokio::test]
    async fn owner_drop_drains_workers_even_when_command_queue_is_full() {
        let home = Home::new();
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let outcome = Arc::new(AtomicUsize::new(0));
        let check_home = home.0.clone();
        let check_started = Arc::clone(&started);
        let check_dropped = Arc::clone(&dropped);
        let check_outcome = Arc::clone(&outcome);
        let check: ProviderCheck = Arc::new(move |_| {
            let probe = LockProbe {
                home: check_home.clone(),
                outcome: Arc::clone(&check_outcome),
                dropped: Arc::clone(&check_dropped),
            };
            let started = Arc::clone(&check_started);
            Box::pin(async move {
                let _probe = probe;
                started.notify_one();
                std::future::pending::<AttemptOutcome>().await
            })
        });
        let state = RuntimeState::open(&home.0).unwrap();
        let (mut owner, client) = start(state, check, wall_clock(Arc::new(AtomicI64::new(1_000))));
        let refresh_client = client.clone();
        let refresh =
            tokio::spawn(async move { refresh_client.refresh(Duration::from_secs(4)).await });
        started.notified().await;

        let mut replies = Vec::new();
        for _ in 0..super::COMMAND_CAPACITY {
            let (reply, response) = oneshot::channel();
            client.admit(Command::Feed(reply)).unwrap();
            replies.push(response);
        }
        let (reply, _) = oneshot::channel();
        assert_eq!(client.admit(Command::Feed(reply)), Err(RequestError::Busy));

        let task = owner.task.take().unwrap();
        drop(owner);
        dropped.notified().await;
        assert_eq!(outcome.load(Ordering::SeqCst), 1);
        task.await.unwrap();
        assert!(RuntimeState::open(&home.0).is_ok());
        assert!(matches!(
            refresh.await.unwrap(),
            Err(RequestError::Unavailable)
        ));
        drop(replies);
    }

    #[tokio::test(start_paused = true)]
    async fn attempts_are_concurrent_and_overlap_coalesces() {
        let both_started = Arc::new(Barrier::new(3));
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let check = {
            let both_started = Arc::clone(&both_started);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            check_with(move |id| {
                let both_started = Arc::clone(&both_started);
                let release = Arc::clone(&release);
                let calls = Arc::clone(&calls);
                async move {
                    if id == "claude" || id == "codex" {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let release_waiter = release.notified();
                        tokio::pin!(release_waiter);
                        release_waiter.as_mut().enable();
                        both_started.wait().await;
                        release_waiter.await;
                        AttemptOutcome::success(observation(wall(500)))
                    } else {
                        AttemptOutcome::failure(ErrorCode::ProviderError)
                    }
                }
            })
        };
        let (_home, owner, client, _) = rig(check, 1_000).await;
        let first = begin(&client).await;
        both_started.wait().await;
        let second = begin(&client).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        release.notify_waiters();
        wait_for_all(first).await;
        wait_for_all(second).await;
        let feed = client.cached().await.unwrap();
        assert_eq!(feed_provider(&feed, "claude").status, "healthy");
        assert_eq!(
            feed_provider(&feed, "claude").captured_at.as_deref(),
            Some("1970-01-01T00:16:40Z")
        );
        assert_eq!(feed_provider(&feed, "codex").status, "healthy");
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn aborted_caller_does_not_cancel_shared_work_and_later_call_joins() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let check = {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            check_with(move |id| {
                let calls = Arc::clone(&calls);
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                async move {
                    if id != "claude" {
                        return AttemptOutcome::failure(ErrorCode::ProviderError);
                    }
                    calls.fetch_add(1, Ordering::SeqCst);
                    let release_waiter = release.notified();
                    tokio::pin!(release_waiter);
                    release_waiter.as_mut().enable();
                    started.notify_one();
                    release_waiter.await;
                    AttemptOutcome::success(observation(wall(0)))
                }
            })
        };
        let (_home, owner, client, _) = rig(check, 1_000).await;
        let started_waiter = started.notified();
        tokio::pin!(started_waiter);
        started_waiter.as_mut().enable();
        let caller = {
            let client = client.clone();
            tokio::spawn(async move { client.refresh(Duration::from_secs(4)).await })
        };
        started_waiter.await;
        caller.abort();
        let joined = begin(&client).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.notify_waiters();
        wait_for_all(joined).await;
        let feed = client.cached().await.unwrap();
        assert_eq!(feed_provider(&feed, "claude").status, "healthy");
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn future_observation_is_permanently_dropped_when_wall_clock_moves_back() {
        let calls = Arc::new(AtomicUsize::new(0));
        let check = check_with({
            let calls = Arc::clone(&calls);
            move |id| {
                let calls = Arc::clone(&calls);
                async move {
                    if id == "claude" {
                        calls.fetch_add(1, Ordering::SeqCst);
                        AttemptOutcome::success(observation(wall(100)))
                    } else {
                        AttemptOutcome::failure(ErrorCode::ProviderError)
                    }
                }
            }
        });
        let (_home, owner, client, clock) = rig(check, 100).await;
        client.refresh(Duration::from_secs(1)).await.unwrap();
        clock.store(99, Ordering::SeqCst);
        let backwards = client.cached().await.unwrap();
        assert_eq!(feed_provider(&backwards, "claude").freshness, "unknown");
        clock.store(101, Ordering::SeqCst);
        let caught_up = client.cached().await.unwrap();
        assert_eq!(feed_provider(&caught_up, "claude").freshness, "unknown");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let restored = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(feed_provider(&restored, "claude").freshness, "fresh");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn successful_cache_expires_at_300_seconds_and_is_not_persisted() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (home, owner, client, clock) = rig(check_success(Arc::clone(&calls)), 1_000).await;
        let initial = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(feed_provider(&initial, "claude").age_seconds, Some(0));

        clock.store(1_299, Ordering::SeqCst);
        client.refresh(Duration::ZERO).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        clock.store(1_300, Ordering::SeqCst);
        client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 10);
        owner.shutdown().await;

        let state = RuntimeState::open(&home.0).unwrap();
        let (owner, restarted) = start(
            state,
            check_success(Arc::clone(&calls)),
            wall_clock(Arc::clone(&clock)),
        );
        let after_restart = restarted.cached().await.unwrap();
        assert_eq!(feed_provider(&after_restart, "claude").freshness, "unknown");
        assert_eq!(calls.load(Ordering::SeqCst), 10);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn configuration_errors_fail_closed_and_recover_after_correction() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (home, owner, client, _) = rig(check_success(Arc::clone(&calls)), 1_000).await;
        write_config(&home, b"{invalid");
        assert_eq!(
            client.refresh(Duration::ZERO).await.unwrap_err(),
            RequestError::Configuration
        );
        assert_eq!(
            client.cached().await.unwrap_err(),
            RequestError::Configuration
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        write_config(
            &home,
            br#"{"disabledProviders":["codex","cursor","opencode-go","muse"]}"#,
        );
        let recovered = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(recovered.disabled_provider_ids.len(), 4);
        assert_eq!(recovered.providers.len(), 1);
        assert_eq!(feed_provider(&recovered, "claude").status, "healthy");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn muse_guard_is_saved_before_check_and_short_retry_does_not_shorten_it() {
        let home = Home::new();
        let state = RuntimeState::open(&home.0).unwrap();
        let seen_guard = Arc::new(AtomicI64::new(0));
        let muse_calls = Arc::new(AtomicUsize::new(0));
        let guard_path = cooldown_path(&home, "muse");
        let check = {
            let seen_guard = Arc::clone(&seen_guard);
            let muse_calls = Arc::clone(&muse_calls);
            check_with(move |id| {
                let seen_guard = Arc::clone(&seen_guard);
                let muse_calls = Arc::clone(&muse_calls);
                let guard_path = guard_path.clone();
                async move {
                    if id == "muse" {
                        muse_calls.fetch_add(1, Ordering::SeqCst);
                        let record: serde_json::Value =
                            serde_json::from_slice(&std::fs::read(guard_path).unwrap()).unwrap();
                        let deadline =
                            DateTime::parse_from_rfc3339(record["notBefore"].as_str().unwrap())
                                .unwrap()
                                .timestamp();
                        seen_guard.store(deadline, Ordering::SeqCst);
                        AttemptOutcome {
                            result: Err(ErrorCode::NetworkError),
                            retry_after: Some(wall(1_060)),
                        }
                    } else {
                        AttemptOutcome::failure(ErrorCode::NetworkError)
                    }
                }
            })
        };
        let clock = Arc::new(AtomicI64::new(1_000));
        let (owner, client) = start(state, check, wall_clock(Arc::clone(&clock)));
        let first = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            feed_provider(&first, "muse").error_code,
            Some(ErrorCode::NetworkError)
        );
        assert_eq!(seen_guard.load(Ordering::SeqCst), 1_300);
        assert_eq!(cooldown_deadline(&home, "muse"), wall(1_300));
        for id in ["claude", "codex", "cursor", "opencode-go"] {
            assert!(
                !cooldown_path(&home, id).exists(),
                "unexpected fixed guard for {id}"
            );
        }

        clock.store(1_299, Ordering::SeqCst);
        let guarded = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            feed_provider(&guarded, "muse").error_code,
            Some(ErrorCode::Cooldown)
        );
        assert_eq!(muse_calls.load(Ordering::SeqCst), 1);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn muse_guard_persistence_failure_prevents_callback() {
        let home = Home::new();
        let state = RuntimeState::open(&home.0).unwrap();
        write_config(
            &home,
            br#"{"disabledProviders":["claude","codex","cursor","opencode-go"]}"#,
        );
        std::fs::create_dir(cooldown_path(&home, "muse")).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let check = check_with({
            let calls = Arc::clone(&calls);
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { AttemptOutcome::failure(ErrorCode::ProviderError) }
            }
        });
        let clock = Arc::new(AtomicI64::new(1_000));
        let (owner, client) = start(state, check, wall_clock(clock));
        let feed = client.refresh(Duration::ZERO).await.unwrap();
        assert_eq!(
            feed_provider(&feed, "muse").error_code,
            Some(ErrorCode::PersistenceError)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn claude_rate_limit_uses_300_second_fallback_without_a_future_header() {
        let home = Home::new();
        let state = RuntimeState::open(&home.0).unwrap();
        write_config(
            &home,
            br#"{"disabledProviders":["codex","cursor","opencode-go","muse"]}"#,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let check = check_with({
            let calls = Arc::clone(&calls);
            move |id| {
                let calls = Arc::clone(&calls);
                async move {
                    if id == "claude" {
                        calls.fetch_add(1, Ordering::SeqCst);
                        AttemptOutcome::failure(ErrorCode::RateLimited)
                    } else {
                        AttemptOutcome::failure(ErrorCode::ProviderError)
                    }
                }
            }
        });
        let clock = Arc::new(AtomicI64::new(1_000));
        let (owner, client) = start(state, check, wall_clock(Arc::clone(&clock)));
        let limited = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            feed_provider(&limited, "claude").error_code,
            Some(ErrorCode::RateLimited)
        );
        assert_eq!(cooldown_deadline(&home, "claude"), wall(1_300));
        let cooldown = client.refresh(Duration::ZERO).await.unwrap();
        assert_eq!(
            feed_provider(&cooldown, "claude").error_code,
            Some(ErrorCode::Cooldown)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn long_retry_after_is_saved_before_response_and_honored_after_restart() {
        let home = Home::new();
        let state = RuntimeState::open(&home.0).unwrap();
        write_config(
            &home,
            br#"{"disabledProviders":["codex","cursor","opencode-go","muse"]}"#,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let check = check_with({
            let calls = Arc::clone(&calls);
            move |id| {
                let calls = Arc::clone(&calls);
                async move {
                    if id == "claude" {
                        calls.fetch_add(1, Ordering::SeqCst);
                        AttemptOutcome {
                            result: Err(ErrorCode::RateLimited),
                            retry_after: Some(wall(1_600)),
                        }
                    } else {
                        AttemptOutcome::failure(ErrorCode::ProviderError)
                    }
                }
            }
        });
        let clock = Arc::new(AtomicI64::new(1_000));
        let (owner, client) = start(state, check, wall_clock(Arc::clone(&clock)));
        client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(cooldown_deadline(&home, "claude"), wall(1_600));
        owner.shutdown().await;

        let state = RuntimeState::open(&home.0).unwrap();
        let (owner, restarted) = start(state, check_success(Arc::clone(&calls)), wall_clock(clock));
        let limited = restarted.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            feed_provider(&limited, "claude").error_code,
            Some(ErrorCode::Cooldown)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn failed_completion_persistence_retries_the_same_deadline_before_work() {
        let home = Home::new();
        let state = RuntimeState::open(&home.0).unwrap();
        let claude_path = cooldown_path(&home, "claude");
        let callback_path = claude_path.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let persisted_before_retry = Arc::new(AtomicI64::new(0));
        let check = {
            let calls = Arc::clone(&calls);
            let persisted_before_retry = Arc::clone(&persisted_before_retry);
            check_with(move |id| {
                let calls = Arc::clone(&calls);
                let persisted_before_retry = Arc::clone(&persisted_before_retry);
                let claude_path = callback_path.clone();
                async move {
                    if id != "claude" {
                        return AttemptOutcome::failure(ErrorCode::NetworkError);
                    }
                    match calls.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            std::fs::create_dir(&claude_path).unwrap();
                            AttemptOutcome {
                                result: Err(ErrorCode::RateLimited),
                                retry_after: Some(wall(1_010)),
                            }
                        }
                        _ => {
                            let record: serde_json::Value =
                                serde_json::from_slice(&std::fs::read(claude_path).unwrap())
                                    .unwrap();
                            let saved =
                                DateTime::parse_from_rfc3339(record["notBefore"].as_str().unwrap())
                                    .unwrap()
                                    .timestamp();
                            persisted_before_retry.store(saved, Ordering::SeqCst);
                            AttemptOutcome::success(observation(wall(1_000)))
                        }
                    }
                }
            })
        };
        let clock = Arc::new(AtomicI64::new(1_000));
        let (owner, client) = start(state, check, wall_clock(Arc::clone(&clock)));
        let obstructed = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            feed_provider(&obstructed, "claude").error_code,
            Some(ErrorCode::PersistenceError)
        );
        std::fs::remove_dir(&claude_path).unwrap();

        clock.store(1_001, Ordering::SeqCst);
        let saved_guard = client.refresh(Duration::ZERO).await.unwrap();
        assert_eq!(
            feed_provider(&saved_guard, "claude").error_code,
            Some(ErrorCode::Cooldown)
        );
        assert_eq!(cooldown_deadline(&home, "claude"), wall(1_010));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        clock.store(1_010, Ordering::SeqCst);
        let recovered = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(feed_provider(&recovered, "claude").status, "healthy");
        assert_eq!(persisted_before_retry.load(Ordering::SeqCst), 1_010);
        assert_eq!(cooldown_deadline(&home, "claude"), wall(1_010));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn provider_panic_is_reported_without_killing_owner() {
        let check = check_with(|id| async move {
            if id == "claude" {
                panic!("injected provider panic");
            }
            AttemptOutcome::failure(ErrorCode::ProviderError)
        });
        let (_home, owner, client, _) = rig(check, 1_000).await;
        let feed = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            feed_provider(&feed, "claude").error_code,
            Some(ErrorCode::InternalError)
        );
        assert_eq!(client.cached().await.unwrap().providers.len(), 5);
        owner.shutdown().await;
    }

    struct DropSignal(Arc<Notify>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_and_drains_workers_before_releasing_file_lock() {
        let home = Home::new();
        let state = RuntimeState::open(&home.0).unwrap();
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let check = {
            let started = Arc::clone(&started);
            let dropped = Arc::clone(&dropped);
            let release = Arc::clone(&release);
            check_with(move |id| {
                let started = Arc::clone(&started);
                let dropped = Arc::clone(&dropped);
                let release = Arc::clone(&release);
                async move {
                    if id != "claude" {
                        return AttemptOutcome::failure(ErrorCode::ProviderError);
                    }
                    let _drop_signal = DropSignal(dropped);
                    let release_waiter = release.notified();
                    tokio::pin!(release_waiter);
                    release_waiter.as_mut().enable();
                    started.notify_one();
                    release_waiter.await;
                    AttemptOutcome::failure(ErrorCode::ProviderError)
                }
            })
        };
        let clock = Arc::new(AtomicI64::new(10));
        let (owner, client) = start(state, check, wall_clock(clock));
        let started_waiter = started.notified();
        tokio::pin!(started_waiter);
        started_waiter.as_mut().enable();
        let completion = begin(&client).await;
        started_waiter.await;
        let dropped_waiter = dropped.notified();
        tokio::pin!(dropped_waiter);
        dropped_waiter.as_mut().enable();
        owner.shutdown().await;
        dropped_waiter.await;
        drop(completion);
        assert_eq!(
            client.cached().await.unwrap_err(),
            RequestError::Unavailable
        );
        drop(client);
        let reopened = RuntimeState::open(&home.0).unwrap();
        drop(reopened);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_and_attempt_timeout_keep_stale_data_and_clear_busy() {
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let check = {
            let calls = Arc::clone(&calls);
            let started = Arc::clone(&started);
            check_with(move |id| {
                let calls = Arc::clone(&calls);
                let started = Arc::clone(&started);
                async move {
                    if id != "claude" {
                        return AttemptOutcome::failure(ErrorCode::ProviderError);
                    }
                    match calls.fetch_add(1, Ordering::SeqCst) {
                        0 => AttemptOutcome::success(observation(wall(1_000))),
                        1 => AttemptOutcome::failure(ErrorCode::NetworkError),
                        _ => {
                            started.notify_one();
                            std::future::pending::<AttemptOutcome>().await
                        }
                    }
                }
            })
        };
        let (_home, owner, client, clock) = rig(check, 1_000).await;
        let first = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(feed_provider(&first, "claude").freshness, "fresh");

        clock.store(1_300, Ordering::SeqCst);
        let failed = client.refresh(Duration::from_secs(1)).await.unwrap();
        assert_eq!(feed_provider(&failed, "claude").freshness, "stale");
        assert_eq!(
            feed_provider(&failed, "claude").error_code,
            Some(ErrorCode::NetworkError)
        );

        clock.store(1_600, Ordering::SeqCst);
        let started_waiter = started.notified();
        tokio::pin!(started_waiter);
        started_waiter.as_mut().enable();
        let timed = {
            let client = client.clone();
            tokio::spawn(async move { client.refresh(Duration::from_millis(100)).await })
        };
        started_waiter.await;
        tokio::time::advance(Duration::from_millis(100)).await;
        let busy = timed.await.unwrap().unwrap();
        assert_eq!(feed_provider(&busy, "claude").freshness, "stale");
        assert_eq!(
            feed_provider(&busy, "claude").error_code,
            Some(ErrorCode::NetworkError)
        );

        let joined = begin(&client).await;
        tokio::time::advance(MAX_ATTEMPT).await;
        wait_for_all(joined).await;
        let timed_out = client.cached().await.unwrap();
        assert_eq!(feed_provider(&timed_out, "claude").freshness, "stale");
        assert_eq!(
            feed_provider(&timed_out, "claude").error_code,
            Some(ErrorCode::NetworkError)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn caller_deadline_returns_busy_while_shared_attempt_finishes_later() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let check = {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            check_with(move |id| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let calls = Arc::clone(&calls);
                async move {
                    if id != "claude" {
                        return AttemptOutcome::failure(ErrorCode::ProviderError);
                    }
                    calls.fetch_add(1, Ordering::SeqCst);
                    let release_waiter = release.notified();
                    tokio::pin!(release_waiter);
                    release_waiter.as_mut().enable();
                    started.notify_one();
                    release_waiter.await;
                    AttemptOutcome::success(observation(wall(10)))
                }
            })
        };
        let (_home, owner, client, _) = rig(check, 10).await;
        let started_waiter = started.notified();
        tokio::pin!(started_waiter);
        started_waiter.as_mut().enable();
        let timed = {
            let client = client.clone();
            tokio::spawn(async move { client.refresh(Duration::from_millis(100)).await })
        };
        started_waiter.await;
        tokio::time::advance(Duration::from_millis(100)).await;
        let busy = timed.await.unwrap().unwrap();
        assert_eq!(
            feed_provider(&busy, "claude").error_code,
            Some(ErrorCode::Busy)
        );

        let joined = begin(&client).await;
        release.notify_waiters();
        wait_for_all(joined).await;
        let final_feed = client.cached().await.unwrap();
        assert_eq!(feed_provider(&final_feed, "claude").status, "healthy");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        owner.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn disable_reenable_discards_held_result_without_parallel_retry() {
        let home = Home::new();
        let state = RuntimeState::open(&home.0).unwrap();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let check = {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            check_with(move |id| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let calls = Arc::clone(&calls);
                async move {
                    if id != "claude" {
                        return AttemptOutcome::failure(ErrorCode::ProviderError);
                    }
                    calls.fetch_add(1, Ordering::SeqCst);
                    let release_waiter = release.notified();
                    tokio::pin!(release_waiter);
                    release_waiter.as_mut().enable();
                    started.notify_one();
                    release_waiter.await;
                    AttemptOutcome {
                        result: Ok(observation(wall(10))),
                        retry_after: Some(wall(610)),
                    }
                }
            })
        };
        let clock = Arc::new(AtomicI64::new(10));
        let (owner, client) = start(state, check, wall_clock(Arc::clone(&clock)));
        let started_waiter = started.notified();
        tokio::pin!(started_waiter);
        started_waiter.as_mut().enable();
        let first = begin(&client).await;
        started_waiter.await;

        write_config(&home, br#"{"disabledProviders":["claude"]}"#);
        let disabled = client.cached().await.unwrap();
        assert!(
            disabled
                .providers
                .iter()
                .all(|provider| provider.id != "claude")
        );
        write_config(&home, br#"{"disabledProviders":[]}"#);
        let joined = begin(&client).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        release.notify_waiters();
        wait_for_all(first).await;
        wait_for_all(joined).await;
        let after = client.cached().await.unwrap();
        assert_eq!(feed_provider(&after, "claude").status, "unknown");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cooldown_deadline(&home, "claude"), wall(610));
        owner.shutdown().await;
    }
}
