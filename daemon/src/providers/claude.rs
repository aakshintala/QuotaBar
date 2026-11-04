use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, Response, Url};
use serde::Deserialize;
use serde_json::Value;

use crate::feed::ErrorCode;
use crate::providers::credential_file::{
    CredentialFileError, JsonSnapshot, ReplaceOutcome, read_json_snapshot, replace_if_unchanged,
};
use crate::providers::http::{build_client, parse_retry_after, read_bounded_body};
use crate::providers::keychain::{
    ClaudeAccount, ClaudeKeychainStore, ClaudeWriteOutcome, KeychainReadError,
    NativeClaudeKeychainStore, SecretBytes,
};
use crate::quota::{Observation, Quota};
use crate::scheduler::AttemptOutcome;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPES: &str = "user:profile user:inference user:sessions:claude_code";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const REFRESH_BUFFER: ChronoDuration = ChronoDuration::minutes(5);

/// Claude API probe. It keeps no result cache or cooldown; the scheduler owns
/// both policies. Credentials are read only when a check is requested.
pub struct ClaudeProbe {
    home: PathBuf,
    client: Client,
    usage_url: Url,
    refresh_url: Url,
    keychain: Arc<dyn ClaudeKeychainStore>,
    account_resolver: Arc<dyn ClaudeAccountResolver>,
}

impl ClaudeProbe {
    /// Construct a production probe using explicit service HOME and a finite,
    /// no-proxy, no-redirect client. Construction performs no credential I/O.
    pub fn new(home: PathBuf) -> Result<Self, ErrorCode> {
        let client = build_client(REQUEST_TIMEOUT, CONNECT_TIMEOUT)?;
        Self::with_client(home, &client)
    }

    /// Construct with a clone of the application-wide client. The client must
    /// already have the shared bounded/no-proxy/no-redirect policy.
    pub(crate) fn with_client(home: PathBuf, client: &Client) -> Result<Self, ErrorCode> {
        Self::with_dependencies(
            home,
            client,
            Arc::new(NativeClaudeKeychainStore),
            Arc::new(EffectiveLoginAccount),
        )
    }

    fn with_dependencies(
        home: PathBuf,
        client: &Client,
        keychain: Arc<dyn ClaudeKeychainStore>,
        account_resolver: Arc<dyn ClaudeAccountResolver>,
    ) -> Result<Self, ErrorCode> {
        Ok(Self {
            home,
            client: client.clone(),
            usage_url: Url::parse(USAGE_URL).map_err(|_| ErrorCode::ConfigurationError)?,
            refresh_url: Url::parse(REFRESH_URL).map_err(|_| ErrorCode::ConfigurationError)?,
            keychain,
            account_resolver,
        })
    }

    /// Refresh the file-backed login when needed, then make at most one usage
    /// retry after a 401. The scheduler assigns the final capture time.
    pub async fn probe(&self) -> AttemptOutcome {
        self.probe_at(Utc::now()).await
    }

    async fn probe_at(&self, captured_at: DateTime<Utc>) -> AttemptOutcome {
        let mut credentials = match load_credentials(
            &self.home,
            self.keychain.as_ref(),
            self.account_resolver.as_ref(),
        ) {
            Ok(credentials) => credentials,
            Err(error) => return AttemptOutcome::failure(error),
        };
        let mut renewal_attempted = false;

        if needs_refresh(&credentials, Utc::now()) && credentials.refresh_token.is_some() {
            match self
                .refresh_if_current(credentials, false, &mut renewal_attempted)
                .await
            {
                Ok(updated) => credentials = updated,
                Err(outcome) => return outcome,
            }
        }

        match self.fetch_usage(&credentials, captured_at).await {
            UsageResult::Success(outcome) => outcome,
            UsageResult::Failure {
                error,
                retry_after,
                unauthorized: true,
            } => {
                // A known future server guard forbids another HTTP step, even
                // when the first response was an otherwise refreshable 401.
                if retry_after.is_some() || renewal_attempted || credentials.refresh_token.is_none()
                {
                    return failure(error, retry_after);
                }

                match self
                    .refresh_if_current(credentials, true, &mut renewal_attempted)
                    .await
                {
                    Ok(updated) => match self.fetch_usage(&updated, Utc::now()).await {
                        UsageResult::Success(outcome) => outcome,
                        UsageResult::Failure {
                            error, retry_after, ..
                        } => failure(error, retry_after),
                    },
                    Err(outcome) => outcome,
                }
            }
            UsageResult::Failure {
                error,
                retry_after,
                unauthorized: false,
            } => failure(error, retry_after),
        }
    }

    async fn refresh_if_current(
        &self,
        mut credentials: Credentials,
        forced: bool,
        renewal_attempted: &mut bool,
    ) -> Result<Credentials, AttemptOutcome> {
        let reloaded = match reload_if_changed(&credentials, self.keychain.as_ref()) {
            Ok(reloaded) => reloaded,
            Err(error) => return Err(AttemptOutcome::failure(error)),
        };
        let changed = reloaded.is_some();
        if let Some(reloaded) = reloaded {
            credentials = reloaded;
        }

        // If the CLI replaced the file while the usage request was in flight,
        // use its fresh token rather than renewing the old refresh token. On a
        // 401 with unchanged bytes, force exactly one refresh even if expiry is
        // not near yet.
        if (changed || !forced) && !needs_refresh(&credentials, Utc::now()) {
            return Ok(credentials);
        }
        if credentials.refresh_token.is_none() {
            return Err(AttemptOutcome::failure(ErrorCode::LoginExpired));
        }
        if *renewal_attempted {
            return Err(AttemptOutcome::failure(ErrorCode::LoginExpired));
        }

        *renewal_attempted = true;
        match self.refresh_once(&credentials).await {
            RefreshResult::Updated(updated, retry_after) => {
                if retry_after.is_some() {
                    // Rotation is already committed. Honor Retry-After without
                    // allowing the follow-on usage request.
                    Err(failure(ErrorCode::RateLimited, retry_after))
                } else {
                    Ok(updated)
                }
            }
            RefreshResult::Changed(reloaded, retry_after) => {
                if retry_after.is_some() {
                    Err(failure(ErrorCode::RateLimited, retry_after))
                } else if needs_refresh(&reloaded, Utc::now()) {
                    // The old refresh request may have rotated credentials;
                    // never issue a second request from this attempt.
                    Err(failure(ErrorCode::LoginExpired, None))
                } else {
                    Ok(reloaded)
                }
            }
            RefreshResult::TerminalFailure(error, retry_after) => Err(failure(error, retry_after)),
            RefreshResult::Failed(error, retry_after) => {
                if retry_after.is_some() || error == ErrorCode::RateLimited {
                    // OAuth 429 is terminal for this attempt. Leave missing or
                    // unusable Retry-After fallback policy to the coordinator,
                    // and do not recover through a concurrently changed file.
                    return Err(failure(error, retry_after));
                }
                match reload_if_changed(&credentials, self.keychain.as_ref()) {
                    Ok(Some(reloaded)) if !needs_refresh(&reloaded, Utc::now()) => Ok(reloaded),
                    Ok(Some(_)) => Err(failure(ErrorCode::LoginExpired, None)),
                    Ok(None) => Err(failure(error, None)),
                    Err(reload_error) => Err(AttemptOutcome::failure(reload_error)),
                }
            }
        }
    }

    async fn refresh_once(&self, credentials: &Credentials) -> RefreshResult {
        let Some(refresh_token) = credentials.refresh_token.as_ref() else {
            return RefreshResult::Failed(ErrorCode::LoginExpired, None);
        };
        let request_body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token.as_str(),
            "client_id": CLIENT_ID,
            "scope": SCOPES,
        });
        let response = match self
            .client
            .post(self.refresh_url.clone())
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .json(&request_body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return RefreshResult::Failed(ErrorCode::NetworkError, None),
        };

        let retry_after = retry_after_at_headers(&response);
        let status = response.status().as_u16();
        // A 429 is terminal even if its body is truncated or oversized. The
        // coordinator owns the missing-header fallback and persisted guard.
        if status == 429 {
            return RefreshResult::Failed(ErrorCode::RateLimited, retry_after);
        }
        if !(200..300).contains(&status) {
            let body = match read_bounded_body(response, MAX_RESPONSE_BYTES).await {
                Ok(body) => body,
                Err(error) => return RefreshResult::Failed(error, retry_after),
            };
            let error = if status == 401 || (status == 400 && is_invalid_grant(&body)) {
                ErrorCode::LoginExpired
            } else {
                ErrorCode::ProviderError
            };
            return RefreshResult::Failed(error, retry_after);
        }

        let body = match read_bounded_body(response, MAX_RESPONSE_BYTES).await {
            Ok(body) => body,
            Err(error) => return RefreshResult::Failed(error, retry_after),
        };
        let reply: OAuthReply = match serde_json::from_slice(&body) {
            Ok(reply) => reply,
            Err(_) => return RefreshResult::Failed(ErrorCode::ProviderError, retry_after),
        };
        let Some(access_token) = reply
            .access_token
            .filter(|token| !token.trim().is_empty() && authorization_header(token).is_some())
        else {
            return RefreshResult::Failed(ErrorCode::ProviderError, retry_after);
        };
        if reply
            .refresh_token
            .as_ref()
            .is_some_and(|token| token.trim().is_empty())
            || reply.expires_in.is_some_and(|seconds| seconds < 0)
        {
            return RefreshResult::Failed(ErrorCode::ProviderError, retry_after);
        }

        self.persist_refresh(
            credentials,
            access_token,
            reply.refresh_token,
            reply.expires_in,
            retry_after,
        )
    }

    fn persist_refresh(
        &self,
        credentials: &Credentials,
        access_token: String,
        rotated_refresh_token: Option<String>,
        expires_in: Option<i64>,
        retry_after: Option<DateTime<Utc>>,
    ) -> RefreshResult {
        let snapshot_bytes = credentials.snapshot.bytes();
        let mut root: Value = match serde_json::from_slice(snapshot_bytes) {
            Ok(root) => root,
            Err(_) => return RefreshResult::Failed(ErrorCode::ProviderError, retry_after),
        };
        let Some(oauth) = root
            .as_object_mut()
            .and_then(|root| root.get_mut("claudeAiOauth"))
            .and_then(Value::as_object_mut)
        else {
            return RefreshResult::Failed(ErrorCode::ProviderError, retry_after);
        };
        oauth.insert("accessToken".into(), Value::String(access_token.clone()));
        if let Some(refresh_token) = rotated_refresh_token.as_ref() {
            oauth.insert("refreshToken".into(), Value::String(refresh_token.clone()));
        }
        if let Some(seconds) = expires_in {
            let Some(milliseconds) = seconds.checked_mul(1_000) else {
                return RefreshResult::Failed(ErrorCode::ProviderError, retry_after);
            };
            let Some(expires_at) = Utc::now().timestamp_millis().checked_add(milliseconds) else {
                return RefreshResult::Failed(ErrorCode::ProviderError, retry_after);
            };
            oauth.insert("expiresAt".into(), Value::from(expires_at));
        }
        let mut updated_bytes = match serde_json::to_vec(&root) {
            Ok(bytes) => bytes,
            Err(_) => return RefreshResult::Failed(ErrorCode::ProviderError, retry_after),
        };
        match &credentials.snapshot {
            CredentialSnapshot::File { path, snapshot } => {
                let outcome = match replace_if_unchanged(
                    path,
                    snapshot.bytes(),
                    &updated_bytes,
                    snapshot.mode(),
                ) {
                    Ok(outcome) => outcome,
                    Err(_) => return RefreshResult::Failed(ErrorCode::ProviderError, retry_after),
                };
                match outcome {
                    ReplaceOutcome::Replaced | ReplaceOutcome::CommittedDurabilityUncertain => {
                        let mode = snapshot.mode();
                        credentials_from_snapshot(CredentialSnapshot::File {
                            path: path.clone(),
                            snapshot: JsonSnapshot::from_replacement(updated_bytes, mode),
                        })
                        .map(|updated| RefreshResult::Updated(updated, retry_after))
                        .unwrap_or_else(|error| RefreshResult::Failed(error, retry_after))
                    }
                    ReplaceOutcome::SourceChanged => match read_credentials_file(path) {
                        Ok(reloaded) => RefreshResult::Changed(reloaded, retry_after),
                        Err(error) => RefreshResult::Failed(error, retry_after),
                    },
                }
            }
            CredentialSnapshot::Keychain { account, snapshot } => {
                let outcome = self.keychain.replace_claude_if_unchanged(
                    account,
                    snapshot.expose(),
                    &updated_bytes,
                );
                updated_bytes.fill(0);
                match outcome {
                    Ok(ClaudeWriteOutcome::Changed(actual)) => {
                        match credentials_from_keychain(account.clone(), actual) {
                            Ok(reloaded) => RefreshResult::Changed(reloaded, retry_after),
                            Err(error) => RefreshResult::Failed(error, retry_after),
                        }
                    }
                    Ok(ClaudeWriteOutcome::Updated(actual)) => {
                        match credentials_from_keychain(account.clone(), actual) {
                            Ok(reloaded) => RefreshResult::Updated(reloaded, retry_after),
                            Err(error) => RefreshResult::TerminalFailure(error, retry_after),
                        }
                    }
                    Err(error) => {
                        RefreshResult::TerminalFailure(map_keychain_error(error), retry_after)
                    }
                }
            }
        }
    }

    async fn fetch_usage(
        &self,
        credentials: &Credentials,
        captured_at: DateTime<Utc>,
    ) -> UsageResult {
        let Some(authorization) = authorization_header(credentials.access_token.as_str()) else {
            return UsageResult::Failure {
                error: ErrorCode::CredentialsMissing,
                retry_after: None,
                unauthorized: false,
            };
        };
        let response = match self
            .client
            .get(self.usage_url.clone())
            .header(AUTHORIZATION, authorization)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("user-agent", "QuotaBar")
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => {
                return UsageResult::Failure {
                    error: ErrorCode::NetworkError,
                    retry_after: None,
                    unauthorized: false,
                };
            }
        };

        let retry_after = retry_after_at_headers(&response);
        let status = response.status().as_u16();
        if status != 200 {
            let (error, unauthorized) = match status {
                401 => (ErrorCode::LoginExpired, true),
                429 => (ErrorCode::RateLimited, false),
                _ => (ErrorCode::ProviderError, false),
            };
            return UsageResult::Failure {
                error,
                retry_after,
                unauthorized,
            };
        }

        let body = match read_bounded_body(response, MAX_RESPONSE_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                return UsageResult::Failure {
                    error,
                    retry_after,
                    unauthorized: false,
                };
            }
        };
        match parse_usage(&body, credentials.tier, captured_at) {
            Ok(observation) => UsageResult::Success(AttemptOutcome {
                result: Ok(observation),
                retry_after,
            }),
            Err(error) => UsageResult::Failure {
                error,
                retry_after,
                unauthorized: false,
            },
        }
    }

    #[cfg(test)]
    fn for_loopback_test(
        home: PathBuf,
        address: std::net::SocketAddr,
        timeout: Duration,
        keychain: Arc<dyn ClaudeKeychainStore>,
        account_resolver: Arc<dyn ClaudeAccountResolver>,
    ) -> Self {
        use std::net::{IpAddr, Ipv4Addr};

        assert_eq!(address.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        let base = format!("http://{address}");
        let client = build_client(timeout, timeout).expect("test client builds");
        Self {
            home,
            client,
            usage_url: Url::parse(&format!("{base}/usage")).expect("loopback URL parses"),
            refresh_url: Url::parse(&format!("{base}/refresh")).expect("loopback URL parses"),
            keychain,
            account_resolver,
        }
    }
}

fn retry_after_at_headers(response: &Response) -> Option<DateTime<Utc>> {
    // `send` resolves at header receipt: take the clock sample before inspecting
    // status or reading a body, including OAuth errors and successful responses.
    let received_at = Utc::now();
    parse_retry_after(response.headers().get("retry-after"), received_at)
}

fn failure(error: ErrorCode, retry_after: Option<DateTime<Utc>>) -> AttemptOutcome {
    AttemptOutcome {
        result: Err(error),
        retry_after,
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
trait ClaudeAccountResolver: Send + Sync {
    fn resolve(&self) -> Result<ClaudeAccount, ErrorCode>;
}

struct EffectiveLoginAccount;

#[cfg(any(target_os = "macos", test))]
fn account_from_lookup(status: i32, name: Option<&[u8]>) -> Result<ClaudeAccount, ErrorCode> {
    const MAX_ACCOUNT_BYTES: usize = 64 * 1024;
    if status != 0 {
        return Err(ErrorCode::CredentialsMissing);
    }
    let name = name.ok_or(ErrorCode::CredentialsMissing)?;
    if name.is_empty() || name.len() > MAX_ACCOUNT_BYTES {
        return Err(ErrorCode::CredentialsMissing);
    }
    let name = std::str::from_utf8(name).map_err(|_| ErrorCode::CredentialsMissing)?;
    ClaudeAccount::from_login_name(name.to_owned()).ok_or(ErrorCode::CredentialsMissing)
}

impl ClaudeAccountResolver for EffectiveLoginAccount {
    fn resolve(&self) -> Result<ClaudeAccount, ErrorCode> {
        #[cfg(target_os = "macos")]
        {
            use std::mem::MaybeUninit;
            use std::ptr;

            const MAX_ACCOUNT_BUFFER: usize = 64 * 1024;
            let mut buffer = vec![0_u8; MAX_ACCOUNT_BUFFER];
            let mut passwd = MaybeUninit::<libc::passwd>::uninit();
            let mut result = ptr::null_mut();
            let status = unsafe {
                libc::getpwuid_r(
                    libc::geteuid(),
                    passwd.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            };
            if status != 0 || result.is_null() {
                return Err(ErrorCode::CredentialsMissing);
            }
            let passwd = unsafe { passwd.assume_init() };
            if passwd.pw_name.is_null() {
                return Err(ErrorCode::CredentialsMissing);
            }
            let buffer_start = buffer.as_ptr() as usize;
            let buffer_end = buffer_start.saturating_add(buffer.len());
            let name_address = passwd.pw_name as usize;
            if !(buffer_start..buffer_end).contains(&name_address) {
                return Err(ErrorCode::CredentialsMissing);
            }
            let offset = name_address - buffer_start;
            let remaining = &buffer[offset..];
            let Some(name_length) = remaining.iter().position(|byte| *byte == 0) else {
                return Err(ErrorCode::CredentialsMissing);
            };
            account_from_lookup(status, Some(&remaining[..name_length]))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(ErrorCode::CredentialsMissing)
        }
    }
}

fn load_credentials(
    home: &Path,
    keychain: &dyn ClaudeKeychainStore,
    account_resolver: &dyn ClaudeAccountResolver,
) -> Result<Credentials, ErrorCode> {
    if !home.is_absolute() {
        return Err(ErrorCode::ConfigurationError);
    }
    let path = credential_path(home);
    match read_json_snapshot(&path, MAX_CREDENTIAL_BYTES) {
        Ok(snapshot) => match credentials_from_snapshot(CredentialSnapshot::File {
            path: path.clone(),
            snapshot,
        }) {
            Ok(credentials) => Ok(credentials),
            Err(_) => fallback_to_keychain(keychain, account_resolver),
        },
        Err(CredentialFileError::Missing) => fallback_to_keychain(keychain, account_resolver),
        Err(_) => Err(unavailable_error()),
    }
}

fn fallback_to_keychain(
    keychain: &dyn ClaudeKeychainStore,
    account_resolver: &dyn ClaudeAccountResolver,
) -> Result<Credentials, ErrorCode> {
    #[cfg(target_os = "macos")]
    {
        let account = account_resolver.resolve()?;
        let snapshot = keychain.read_claude(&account).map_err(map_keychain_error)?;
        credentials_from_snapshot(CredentialSnapshot::Keychain { account, snapshot })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (keychain, account_resolver);
        Err(ErrorCode::CredentialsMissing)
    }
}

fn read_credentials_file(path: &Path) -> Result<Credentials, ErrorCode> {
    let snapshot =
        read_json_snapshot(path, MAX_CREDENTIAL_BYTES).map_err(|_| unavailable_error())?;
    credentials_from_snapshot(CredentialSnapshot::File {
        path: path.to_path_buf(),
        snapshot,
    })
}

fn credentials_from_keychain(
    account: ClaudeAccount,
    snapshot: SecretBytes,
) -> Result<Credentials, ErrorCode> {
    credentials_from_snapshot(CredentialSnapshot::Keychain { account, snapshot })
}

fn credentials_from_snapshot(source: CredentialSnapshot) -> Result<Credentials, ErrorCode> {
    let root: Value = serde_json::from_slice(source.bytes()).map_err(|_| unavailable_error())?;
    let Some(oauth) = root.get("claudeAiOauth").and_then(Value::as_object) else {
        return Err(unavailable_error());
    };
    let Some(access) = oauth.get("accessToken").and_then(Value::as_str) else {
        return Err(unavailable_error());
    };
    let access = access.trim();
    if access.is_empty() || authorization_header(access).is_none() {
        return Err(unavailable_error());
    }
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .map(|token| Secret::new(token.to_owned()));
    let expires_at_ms = oauth
        .get("expiresAt")
        .and_then(Value::as_f64)
        .filter(|expires_at| expires_at.is_finite());
    let tier = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .and_then(safe_tier);

    Ok(Credentials {
        access_token: Secret::new(access.to_owned()),
        refresh_token,
        expires_at_ms,
        tier,
        snapshot: source,
    })
}

fn reload_if_changed(
    credentials: &Credentials,
    keychain: &dyn ClaudeKeychainStore,
) -> Result<Option<Credentials>, ErrorCode> {
    match &credentials.snapshot {
        CredentialSnapshot::File { path, snapshot } => {
            let current =
                read_json_snapshot(path, MAX_CREDENTIAL_BYTES).map_err(|_| unavailable_error())?;
            if current.bytes() == snapshot.bytes() && current.mode() == snapshot.mode() {
                return Ok(None);
            }
            credentials_from_snapshot(CredentialSnapshot::File {
                path: path.clone(),
                snapshot: current,
            })
            .map(Some)
        }
        CredentialSnapshot::Keychain { account, snapshot } => {
            let current = keychain.read_claude(account).map_err(map_keychain_error)?;
            if current.expose() == snapshot.expose() {
                return Ok(None);
            }
            credentials_from_keychain(account.clone(), current).map(Some)
        }
    }
}

fn credential_path(home: &Path) -> PathBuf {
    home.join(".claude").join(".credentials.json")
}

fn needs_refresh(credentials: &Credentials, now: DateTime<Utc>) -> bool {
    let Some(expires_at_ms) = credentials.expires_at_ms else {
        return true;
    };
    now.timestamp_millis() as f64 + REFRESH_BUFFER.num_milliseconds() as f64 >= expires_at_ms
}

fn authorization_header(token: &str) -> Option<HeaderValue> {
    let authorization = format!("Bearer {}", token.trim());
    let mut header = HeaderValue::from_bytes(authorization.as_bytes()).ok()?;
    header.set_sensitive(true);
    Some(header)
}

fn safe_tier(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "claude_max" | "max" => Some("MAX"),
        "claude_pro" | "pro" => Some("PRO"),
        "claude_api" | "api" => Some("API"),
        _ => None,
    }
}

fn map_keychain_error(error: KeychainReadError) -> ErrorCode {
    match error {
        KeychainReadError::ItemNotFound => ErrorCode::CredentialsMissing,
        KeychainReadError::InteractionUnavailable => ErrorCode::KeychainBlocked,
        KeychainReadError::CredentialTooLarge | KeychainReadError::ReadFailed => {
            ErrorCode::KeychainBlocked
        }
    }
}

fn unavailable_error() -> ErrorCode {
    #[cfg(target_os = "macos")]
    {
        ErrorCode::KeychainBlocked
    }
    #[cfg(not(target_os = "macos"))]
    {
        ErrorCode::CredentialsMissing
    }
}

fn parse_usage(
    input: &[u8],
    tier: Option<&'static str>,
    captured_at: DateTime<Utc>,
) -> Result<Observation, ErrorCode> {
    let response: UsageResponse =
        serde_json::from_slice(input).map_err(|_| ErrorCode::ProviderError)?;
    let mut quotas = Vec::new();
    append_legacy_window(
        &mut quotas,
        response.five_hour,
        "session",
        "Session",
        5 * 60 * 60,
    )?;
    append_legacy_window(
        &mut quotas,
        response.seven_day,
        "weekly",
        "Weekly",
        7 * 24 * 60 * 60,
    )?;
    append_legacy_window(
        &mut quotas,
        response.seven_day_sonnet,
        "model:sonnet",
        "Sonnet",
        7 * 24 * 60 * 60,
    )?;
    append_legacy_window(
        &mut quotas,
        response.seven_day_opus,
        "model:opus",
        "Opus",
        7 * 24 * 60 * 60,
    )?;

    for entry in response.limits.unwrap_or_default() {
        if entry.kind.as_deref() != Some("weekly_scoped") {
            continue;
        }
        let Some(display_name) = entry
            .scope
            .and_then(|scope| scope.model)
            .and_then(|model| model.display_name)
        else {
            continue;
        };
        let Some(model_name) = scoped_model_key(&display_name) else {
            continue;
        };
        let key = format!("model:{model_name}");
        if quotas.iter().any(|quota| quota.key == key) {
            continue;
        }
        let Some(utilization) = entry.percent else {
            continue;
        };
        append_window(
            &mut quotas,
            utilization,
            entry.resets_at.as_deref(),
            key,
            titlecase_ascii(&model_name),
            7 * 24 * 60 * 60,
        )?;
    }

    Ok(Observation {
        captured_at,
        tier: tier.map(str::to_owned),
        quotas,
    })
}

fn append_legacy_window(
    quotas: &mut Vec<Quota>,
    window: Option<UsageWindow>,
    key: &str,
    label: &str,
    duration: i64,
) -> Result<(), ErrorCode> {
    let Some(window) = window else {
        return Ok(());
    };
    let Some(utilization) = window.utilization else {
        return Ok(());
    };
    append_window(
        quotas,
        utilization,
        window.resets_at.as_deref(),
        key.to_owned(),
        label.to_owned(),
        duration,
    )
}

fn append_window(
    quotas: &mut Vec<Quota>,
    utilization: f64,
    reset: Option<&str>,
    key: String,
    label: String,
    duration: i64,
) -> Result<(), ErrorCode> {
    if !utilization.is_finite() {
        return Err(ErrorCode::ProviderError);
    }
    let resets_at = reset
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|date| date.with_timezone(&Utc))
                .map_err(|_| ErrorCode::ProviderError)
        })
        .transpose()?;
    quotas.push(Quota {
        key,
        label,
        percent_remaining: Some((100.0 - utilization).min(100.0)),
        balance_remaining: None,
        balance_used: None,
        balance_cap: None,
        balance_unit: None,
        units_used: None,
        units_limit: None,
        resets_at,
        window_duration_seconds: Some(duration),
    });
    Ok(())
}

fn scoped_model_key(display_name: &str) -> Option<String> {
    let first = display_name.split_whitespace().next()?;
    if first.is_empty()
        || first.len() > 64
        || !first
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(first.to_ascii_lowercase())
}

fn titlecase_ascii(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn is_invalid_grant(body: &[u8]) -> bool {
    serde_json::from_slice::<OAuthError>(body)
        .ok()
        .and_then(|error| error.error)
        .is_some_and(|error| error == "invalid_grant")
}

struct Secret(String);

impl Secret {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

enum CredentialSnapshot {
    File {
        path: PathBuf,
        snapshot: JsonSnapshot,
    },
    Keychain {
        account: ClaudeAccount,
        snapshot: SecretBytes,
    },
}

impl CredentialSnapshot {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::File { snapshot, .. } => snapshot.bytes(),
            Self::Keychain { snapshot, .. } => snapshot.expose(),
        }
    }
}

struct Credentials {
    access_token: Secret,
    refresh_token: Option<Secret>,
    expires_at_ms: Option<f64>,
    tier: Option<&'static str>,
    snapshot: CredentialSnapshot,
}

#[derive(Deserialize)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    seven_day_sonnet: Option<UsageWindow>,
    seven_day_opus: Option<UsageWindow>,
    limits: Option<Vec<LimitEntry>>,
}

#[derive(Deserialize)]
struct LimitEntry {
    kind: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    scope: Option<LimitScope>,
}

#[derive(Deserialize)]
struct LimitScope {
    model: Option<LimitModel>,
}

#[derive(Deserialize)]
struct LimitModel {
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct OAuthReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: Option<String>,
}

enum UsageResult {
    Success(AttemptOutcome),
    Failure {
        error: ErrorCode,
        retry_after: Option<DateTime<Utc>>,
        unauthorized: bool,
    },
}

enum RefreshResult {
    Updated(Credentials, Option<DateTime<Utc>>),
    Changed(Credentials, Option<DateTime<Utc>>),
    Failed(ErrorCode, Option<DateTime<Utc>>),
    TerminalFailure(ErrorCode, Option<DateTime<Utc>>),
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeAccount, ClaudeKeychainStore, ClaudeWriteOutcome, KeychainReadError,
        MAX_CREDENTIAL_BYTES, MAX_RESPONSE_BYTES, OAuthReply, RefreshResult, Secret, SecretBytes,
        account_from_lookup, authorization_header, credential_path, load_credentials, parse_usage,
        safe_tier,
    };
    use crate::feed::ErrorCode;
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    use serde_json::{Value, json};
    use std::fs;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    const USAGE_FULL: &str = r#"{
      "five_hour": {"utilization": 25.5, "resets_at": "2027-01-15T13:00:00.125Z"},
      "seven_day": {"utilization": 45.0, "resets_at": "2027-01-21T08:00:00Z"},
      "seven_day_sonnet": {"utilization": 30.0, "resets_at": "2027-01-21T08:00:00Z"},
      "seven_day_opus": {"utilization": 110.0, "resets_at": "2027-01-21T08:00:00Z"},
      "limits": [
        {"kind":"session","percent":25,"resets_at":"2027-01-15T13:00:00Z","scope":null},
        {"kind":"weekly_all","percent":40,"resets_at":"2027-01-21T08:00:00Z","scope":null},
        {"kind":"weekly_scoped","percent":17,"resets_at":"2027-01-21T08:00:00Z","scope":{"model":{"display_name":"Fable 5"}}},
        {"kind":"weekly_scoped","percent":19,"resets_at":"2027-01-21T08:00:00Z","scope":{"model":{"display_name":"Fable"}}}
      ],
      "spend":{"enabled":true,"used":{"amount_minor":1}},
      "unknown":{"retained-by-swift-but-not-exposed":true}
    }"#;

    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Self {
            let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quotabard-claude-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, private_dir_permissions()).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn credential_path(&self) -> PathBuf {
            credential_path(self.path())
        }

        fn write_credentials(&self, bytes: &[u8]) {
            let path = self.credential_path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::set_permissions(path.parent().unwrap(), private_dir_permissions()).unwrap();
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, private_file_permissions()).unwrap();
        }

        fn credentials(
            &self,
            access: &str,
            refresh: Option<&str>,
            expires_at_ms: Option<i64>,
            tier: &str,
        ) -> Vec<u8> {
            let mut oauth = json!({
                "accessToken": access,
                "subscriptionType": tier,
                "nestedUnknown": {"keep": "yes"},
            });
            if let Some(refresh) = refresh {
                oauth["refreshToken"] = Value::String(refresh.to_owned());
            }
            if let Some(expires_at_ms) = expires_at_ms {
                oauth["expiresAt"] = Value::from(expires_at_ms);
            }
            serde_json::to_vec(&json!({
                "claudeAiOauth": oauth,
                "topUnknown": {"keep": true},
            }))
            .unwrap()
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct FakeKeychainState {
        bytes: Option<Vec<u8>>,
        reads: usize,
        writes: usize,
        replace_after_write: Option<Vec<u8>>,
        fail_read_after_write: bool,
        readback_override: Option<Vec<u8>>,
        last_account: Option<ClaudeAccount>,
    }

    #[derive(Default)]
    struct TestClaudeKeychain(std::sync::Mutex<FakeKeychainState>);

    impl TestClaudeKeychain {
        fn with_record(bytes: Vec<u8>) -> Self {
            Self(std::sync::Mutex::new(FakeKeychainState {
                bytes: Some(bytes),
                ..FakeKeychainState::default()
            }))
        }

        fn read_count(&self) -> usize {
            self.0.lock().unwrap().reads
        }

        fn last_account(&self) -> Option<ClaudeAccount> {
            self.0.lock().unwrap().last_account.clone()
        }

        fn replace_after_write_with(&self, bytes: Vec<u8>) {
            self.0.lock().unwrap().replace_after_write = Some(bytes);
        }

        fn fail_read_after_write(&self) {
            self.0.lock().unwrap().fail_read_after_write = true;
        }

        #[cfg(target_os = "macos")]
        fn return_malformed_after_write(&self) {
            self.0.lock().unwrap().readback_override = Some(b"not-json".to_vec());
        }
    }

    impl ClaudeKeychainStore for TestClaudeKeychain {
        fn read_claude(&self, account: &ClaudeAccount) -> Result<SecretBytes, KeychainReadError> {
            let mut state = self.0.lock().unwrap();
            state.last_account = Some(account.clone());
            state.reads += 1;
            state
                .bytes
                .as_ref()
                .map(|bytes| SecretBytes::from_bytes(bytes.clone()))
                .ok_or(KeychainReadError::ItemNotFound)
        }

        fn replace_claude_if_unchanged(
            &self,
            account: &ClaudeAccount,
            expected: &[u8],
            replacement: &[u8],
        ) -> Result<ClaudeWriteOutcome, KeychainReadError> {
            let mut state = self.0.lock().unwrap();
            state.last_account = Some(account.clone());
            let Some(current) = state.bytes.as_ref() else {
                return Err(KeychainReadError::ItemNotFound);
            };
            if current != expected {
                return Ok(ClaudeWriteOutcome::Changed(SecretBytes::from_bytes(
                    current.clone(),
                )));
            }
            state.writes += 1;
            state.bytes = Some(replacement.to_vec());
            if let Some(concurrent) = state.replace_after_write.take() {
                state.bytes = Some(concurrent);
            }
            if state.fail_read_after_write {
                return Err(KeychainReadError::ReadFailed);
            }
            Ok(ClaudeWriteOutcome::Updated(SecretBytes::from_bytes(
                state
                    .readback_override
                    .take()
                    .unwrap_or_else(|| state.bytes.as_ref().unwrap().clone()),
            )))
        }
    }

    struct FixedAccountResolver;

    impl super::ClaudeAccountResolver for FixedAccountResolver {
        fn resolve(&self) -> Result<ClaudeAccount, ErrorCode> {
            ClaudeAccount::from_login_name("synthetic-login".to_owned())
                .ok_or(ErrorCode::CredentialsMissing)
        }
    }

    fn private_dir_permissions() -> fs::Permissions {
        permissions(0o700)
    }

    fn private_file_permissions() -> fs::Permissions {
        permissions(0o600)
    }

    #[cfg(unix)]
    fn permissions(mode: u32) -> fs::Permissions {
        use std::os::unix::fs::PermissionsExt;
        fs::Permissions::from_mode(mode)
    }

    #[cfg(not(unix))]
    fn permissions(_mode: u32) -> fs::Permissions {
        fs::Permissions::readonly()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn future_expiry() -> i64 {
        Utc::now().timestamp_millis() + ChronoDuration::hours(1).num_milliseconds()
    }

    fn response(status: u16, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!(
            "HTTP/1.1 {status} test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    struct Reply {
        bytes: Vec<u8>,
        before: Option<Box<dyn FnOnce() + Send>>,
    }

    impl Reply {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes,
                before: None,
            }
        }

        fn before(mut self, operation: impl FnOnce() + Send + 'static) -> Self {
            self.before = Some(Box::new(operation));
            self
        }
    }

    struct CapturedRequest {
        line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl CapturedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    async fn server(
        replies: Vec<Reply>,
    ) -> (SocketAddr, tokio::task::JoinHandle<Vec<CapturedRequest>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request_line = line.trim().to_owned();
                let mut headers = Vec::new();
                let mut content_length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        let name = name.trim().to_owned();
                        let value = value.trim().to_owned();
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.parse().unwrap_or(0);
                        }
                        headers.push((name, value));
                    }
                }
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).await.unwrap();
                requests.push(CapturedRequest {
                    line: request_line,
                    headers,
                    body,
                });
                let mut stream = reader.into_inner();
                if let Some(before) = reply.before {
                    before();
                }
                let _ = stream.write_all(&reply.bytes).await;
            }
            requests
        });
        (address, task)
    }

    fn test_dependencies() -> (Arc<TestClaudeKeychain>, Arc<FixedAccountResolver>) {
        (
            Arc::new(TestClaudeKeychain::default()),
            Arc::new(FixedAccountResolver),
        )
    }

    fn load_test_credentials(home: &TestHome) -> Result<super::Credentials, ErrorCode> {
        let (keychain, account) = test_dependencies();
        super::load_credentials(home.path(), keychain.as_ref(), account.as_ref())
    }

    fn probe(home: &TestHome, address: SocketAddr) -> super::ClaudeProbe {
        let (keychain, account) = test_dependencies();
        super::ClaudeProbe::for_loopback_test(
            home.path().to_path_buf(),
            address,
            Duration::from_secs(2),
            keychain,
            account,
        )
    }

    #[test]
    fn effective_login_lookup_rejects_missing_error_oversized_and_invalid_names() {
        assert!(account_from_lookup(0, Some(b"synthetic-user")).is_ok());
        assert!(account_from_lookup(1, Some(b"synthetic-user")).is_err());
        assert!(account_from_lookup(0, None).is_err());
        assert!(account_from_lookup(0, Some(b"")).is_err());
        assert!(account_from_lookup(0, Some(&[b'a'; 64 * 1024 + 1])).is_err());
        assert!(account_from_lookup(0, Some(&[0xff])).is_err());
        assert!(account_from_lookup(0, Some(b"name\nwith-control")).is_err());
    }

    #[test]
    fn explicit_home_path_and_known_tiers_are_bounded() {
        assert_eq!(
            credential_path(Path::new("/synthetic-home")),
            Path::new("/synthetic-home/.claude/.credentials.json")
        );
        assert_eq!(safe_tier("claude_max"), Some("MAX"));
        assert_eq!(safe_tier("claude_pro"), Some("PRO"));
        assert_eq!(safe_tier("api"), Some("API"));
        assert_eq!(safe_tier("account-plus-private-metadata"), None);
    }

    #[test]
    fn parser_preserves_legacy_scoped_resets_deduplication_and_over_quota() {
        let observation = parse_usage(USAGE_FULL.as_bytes(), Some("PRO"), now()).unwrap();
        assert_eq!(observation.tier.as_deref(), Some("PRO"));
        assert_eq!(observation.quotas.len(), 5);
        assert_eq!(observation.quotas[0].key, "session");
        assert_eq!(observation.quotas[0].label, "Session");
        assert_eq!(observation.quotas[0].percent_remaining, Some(74.5));
        assert_eq!(
            observation.quotas[0].resets_at.unwrap().to_rfc3339(),
            "2027-01-15T13:00:00.125+00:00"
        );
        assert_eq!(observation.quotas[1].key, "weekly");
        assert_eq!(observation.quotas[2].key, "model:sonnet");
        assert_eq!(observation.quotas[2].percent_remaining, Some(70.0));
        assert_eq!(observation.quotas[3].key, "model:opus");
        assert_eq!(observation.quotas[3].percent_remaining, Some(-10.0));
        assert_eq!(observation.quotas[4].key, "model:fable");
        assert_eq!(observation.quotas[4].label, "Fable");
        assert!(
            observation
                .quotas
                .iter()
                .all(|quota| quota.balance_remaining.is_none())
        );
        assert!(
            observation
                .quotas
                .iter()
                .all(
                    |quota| quota.window_duration_seconds == Some(7 * 24 * 60 * 60)
                        || quota.key == "session"
                )
        );
    }

    #[test]
    fn parser_accepts_empty_and_partial_responses_and_ignores_unscoped_limits() {
        for source in [
            "{}",
            r#"{"five_hour":{"utilization":null,"resets_at":"not-a-date"},"seven_day":null}"#,
            r#"{"limits":[{"kind":"session","percent":1,"resets_at":"bad","scope":null},{"kind":"weekly_all","percent":1,"resets_at":"bad","scope":null}]}"#,
        ] {
            let result = parse_usage(source.as_bytes(), None, now()).unwrap();
            assert!(result.quotas.is_empty());
        }
        let partial = parse_usage(
            br#"{"five_hour":{"utilization":0,"resets_at":null},"seven_day":{"utilization":-20},"seven_day_sonnet":{"utilization":110}}"#,
            None,
            now(),
        )
        .unwrap();
        assert_eq!(partial.quotas.len(), 3);
        assert_eq!(partial.quotas[0].percent_remaining, Some(100.0));
        assert_eq!(partial.quotas[1].percent_remaining, Some(100.0));
        assert_eq!(partial.quotas[2].percent_remaining, Some(-10.0));
        assert!(partial.quotas[2].resets_at.is_none());
    }

    #[test]
    fn parser_rejects_invalid_used_window_timestamps_and_non_finite_values() {
        for source in [
            r#"{"five_hour":{"utilization":1,"resets_at":"tomorrow"}}"#,
            r#"{"five_hour":{"utilization":1e999}}"#,
            r#"{"limits":[{"kind":"weekly_scoped","percent":1,"resets_at":"invalid","scope":{"model":{"display_name":"Fable"}}}]}"#,
            "not-json",
        ] {
            assert_eq!(
                parse_usage(source.as_bytes(), None, now()).unwrap_err(),
                ErrorCode::ProviderError
            );
        }
    }

    #[test]
    fn parser_skips_bad_model_scope_and_keeps_first_duplicate() {
        let source = br#"{"limits":[
          {"kind":"weekly_scoped","percent":105,"scope":{"model":{"display_name":"Fable 5"}}},
          {"kind":"weekly_scoped","percent":20,"scope":{"model":{"display_name":"Fable"}}},
          {"kind":"weekly_scoped","percent":30,"scope":{"model":{"display_name":"bad!name"}}}
        ]}"#;
        let parsed = parse_usage(source, None, now()).unwrap();
        assert_eq!(parsed.quotas.len(), 1);
        assert_eq!(parsed.quotas[0].key, "model:fable");
        assert_eq!(parsed.quotas[0].percent_remaining, Some(-5.0));
    }

    #[test]
    fn credentials_use_only_explicit_temp_home_and_safe_tier() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "synthetic-access",
            Some("synthetic-refresh"),
            Some(future_expiry()),
            "claude_max",
        ));
        let loaded = load_test_credentials(&home).unwrap();
        assert_eq!(loaded.access_token.as_str(), "synthetic-access");
        assert_eq!(
            loaded.refresh_token.as_ref().map(Secret::as_str),
            Some("synthetic-refresh")
        );
        assert_eq!(loaded.tier, Some("MAX"));
    }

    #[test]
    fn keychain_refresh_uses_the_whole_current_record_after_a_concurrent_replacement() {
        let home = TestHome::new();
        let initial = home.credentials("old-access", Some("old-refresh"), Some(0), "claude_pro");
        let concurrent = home.credentials(
            "cli-replacement",
            Some("cli-refresh"),
            Some(future_expiry()),
            "claude_max",
        );
        let keychain = Arc::new(TestClaudeKeychain::with_record(initial.clone()));
        keychain.replace_after_write_with(concurrent);
        let account = ClaudeAccount::from_login_name("synthetic-login".to_owned()).unwrap();
        let credentials =
            super::credentials_from_keychain(account, SecretBytes::from_bytes(initial)).unwrap();
        let probe = super::ClaudeProbe::with_dependencies(
            home.path().to_path_buf(),
            &reqwest::Client::new(),
            keychain.clone(),
            Arc::new(FixedAccountResolver),
        )
        .unwrap();

        let result = probe.persist_refresh(
            &credentials,
            "renewed-access".to_owned(),
            Some("renewed-refresh".to_owned()),
            Some(3_600),
            None,
        );
        match result {
            RefreshResult::Updated(actual, None) => {
                assert_eq!(actual.access_token.as_str(), "cli-replacement");
                assert_eq!(
                    actual.refresh_token.as_ref().unwrap().as_str(),
                    "cli-refresh"
                );
                assert_eq!(actual.tier, Some("MAX"));
            }
            _ => panic!("expected the complete concurrent record to win"),
        }
        assert_eq!(keychain.0.lock().unwrap().writes, 1);
    }

    #[test]
    fn keychain_change_before_modify_wins_without_a_write() {
        let home = TestHome::new();
        let initial = home.credentials("old-access", Some("old-refresh"), Some(0), "pro");
        let concurrent = home.credentials(
            "cli-access",
            Some("cli-refresh"),
            Some(future_expiry()),
            "claude_max",
        );
        let keychain = Arc::new(TestClaudeKeychain::with_record(initial.clone()));
        keychain.0.lock().unwrap().bytes = Some(concurrent);
        let account = ClaudeAccount::from_login_name("synthetic-login".to_owned()).unwrap();
        let credentials =
            super::credentials_from_keychain(account, SecretBytes::from_bytes(initial)).unwrap();
        let probe = super::ClaudeProbe::with_dependencies(
            home.path().to_path_buf(),
            &reqwest::Client::new(),
            keychain.clone(),
            Arc::new(FixedAccountResolver),
        )
        .unwrap();

        match probe.persist_refresh(
            &credentials,
            "stale-access".to_owned(),
            None,
            Some(3_600),
            None,
        ) {
            RefreshResult::Changed(actual, None) => {
                assert_eq!(actual.access_token.as_str(), "cli-access");
                assert_eq!(
                    actual.refresh_token.as_ref().unwrap().as_str(),
                    "cli-refresh"
                );
                assert_eq!(actual.tier, Some("MAX"));
            }
            _ => panic!("expected current CLI credentials"),
        }
        assert_eq!(keychain.0.lock().unwrap().writes, 0);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn transient_post_write_readback_failure_stops_proactive_usage_recovery() {
        let home = TestHome::new();
        let initial = home.credentials("old", Some("refresh"), Some(0), "pro");
        let keychain = Arc::new(TestClaudeKeychain::with_record(initial));
        keychain.fail_read_after_write();
        let account = Arc::new(FixedAccountResolver);
        let (address, task) = server(vec![Reply::new(response(
            200,
            "",
            br#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
        ))])
        .await;
        let probe = super::ClaudeProbe::for_loopback_test(
            home.path().to_path_buf(),
            address,
            Duration::from_secs(2),
            keychain.clone(),
            account,
        );

        assert_eq!(
            probe.probe_at(now()).await.result.unwrap_err(),
            ErrorCode::KeychainBlocked
        );
        assert_eq!(task.await.unwrap().len(), 1);
        let state = keychain.0.lock().unwrap();
        assert_eq!(state.writes, 1);
        assert!(
            state
                .bytes
                .as_ref()
                .unwrap()
                .windows(10)
                .any(|part| part == b"new-access")
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn malformed_post_write_readback_stops_401_usage_retry_recovery() {
        let home = TestHome::new();
        let initial = home.credentials("old", Some("refresh"), Some(future_expiry()), "pro");
        let keychain = Arc::new(TestClaudeKeychain::with_record(initial));
        keychain.return_malformed_after_write();
        let (address, task) = server(vec![
            Reply::new(response(401, "", b"ignored")),
            Reply::new(response(
                200,
                "",
                br#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            )),
        ])
        .await;
        let probe = super::ClaudeProbe::for_loopback_test(
            home.path().to_path_buf(),
            address,
            Duration::from_secs(2),
            keychain.clone(),
            Arc::new(FixedAccountResolver),
        );

        assert_eq!(
            probe.probe_at(now()).await.result.unwrap_err(),
            ErrorCode::KeychainBlocked
        );
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].line, "GET /usage HTTP/1.1");
        assert_eq!(requests[1].line, "POST /refresh HTTP/1.1");
        let state = keychain.0.lock().unwrap();
        assert_eq!(state.writes, 1);
        assert!(
            state
                .bytes
                .as_ref()
                .unwrap()
                .windows(10)
                .any(|part| part == b"new-access")
        );
    }

    #[test]
    fn keychain_readback_failure_is_terminal_without_a_stale_retry() {
        let home = TestHome::new();
        let initial = home.credentials("old-access", Some("old-refresh"), Some(0), "pro");
        let keychain = Arc::new(TestClaudeKeychain::with_record(initial.clone()));
        keychain.fail_read_after_write();
        let account = ClaudeAccount::from_login_name("synthetic-login".to_owned()).unwrap();
        let credentials =
            super::credentials_from_keychain(account, SecretBytes::from_bytes(initial)).unwrap();
        let probe = super::ClaudeProbe::with_dependencies(
            home.path().to_path_buf(),
            &reqwest::Client::new(),
            keychain.clone(),
            Arc::new(FixedAccountResolver),
        )
        .unwrap();

        assert!(matches!(
            probe.persist_refresh(
                &credentials,
                "new-access".to_owned(),
                None,
                Some(3_600),
                None
            ),
            RefreshResult::TerminalFailure(ErrorCode::KeychainBlocked, None)
        ));
        assert_eq!(keychain.0.lock().unwrap().writes, 1);
    }

    #[test]
    fn committed_file_refresh_returns_its_snapshot_without_reopening_the_path() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials("old", Some("refresh"), Some(0), "pro"));
        let (keychain, account) = test_dependencies();
        let credentials =
            load_credentials(home.path(), keychain.as_ref(), account.as_ref()).unwrap();
        let probe = super::ClaudeProbe::with_dependencies(
            home.path().to_path_buf(),
            &reqwest::Client::new(),
            keychain,
            account,
        )
        .unwrap();

        let result = probe.persist_refresh(
            &credentials,
            "committed-access".to_owned(),
            None,
            Some(3_600),
            None,
        );
        fs::remove_file(home.credential_path()).unwrap();
        match result {
            RefreshResult::Updated(updated, None) => {
                assert_eq!(updated.access_token.as_str(), "committed-access");
                assert!(
                    updated
                        .snapshot
                        .bytes()
                        .windows(b"committed-access".len())
                        .any(|part| part == b"committed-access")
                );
            }
            _ => panic!("a committed file refresh must not depend on a later path read"),
        }
    }

    #[test]
    fn valid_file_takes_precedence_over_an_available_fake_keychain_record() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "file-token",
            Some("file-refresh"),
            Some(future_expiry()),
            "claude_pro",
        ));
        let keychain = TestClaudeKeychain::with_record(home.credentials(
            "keychain-token",
            Some("keychain-refresh"),
            Some(future_expiry()),
            "claude_max",
        ));
        let account = FixedAccountResolver;
        let credentials = load_credentials(home.path(), &keychain, &account).unwrap();
        assert_eq!(credentials.access_token.as_str(), "file-token");
        assert_eq!(keychain.read_count(), 0);
        assert!(keychain.last_account().is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn absent_file_falls_back_to_the_fixed_account_fake_store() {
        let home = TestHome::new();
        let keychain = TestClaudeKeychain::with_record(home.credentials(
            "keychain-token",
            Some("keychain-refresh"),
            Some(future_expiry()),
            "claude_max",
        ));
        let account = FixedAccountResolver;
        let credentials = load_credentials(home.path(), &keychain, &account).unwrap();
        assert_eq!(credentials.access_token.as_str(), "keychain-token");
        assert_eq!(keychain.read_count(), 1);
        assert!(
            keychain.last_account() == ClaudeAccount::from_login_name("synthetic-login".to_owned())
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn absent_file_remains_file_only_on_linux() {
        let home = TestHome::new();
        let keychain = TestClaudeKeychain::with_record(home.credentials(
            "keychain-token",
            Some("keychain-refresh"),
            Some(future_expiry()),
            "claude_max",
        ));
        let account = FixedAccountResolver;
        assert_eq!(
            load_credentials(home.path(), &keychain, &account)
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
        assert_eq!(keychain.read_count(), 0);
    }

    #[test]
    fn missing_or_unusable_file_uses_only_the_injected_store_fallback() {
        let home = TestHome::new();
        let (store, account) = test_dependencies();
        assert_eq!(
            load_credentials(home.path(), store.as_ref(), account.as_ref())
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );

        home.write_credentials(b"not-json");
        assert_eq!(
            load_credentials(home.path(), store.as_ref(), account.as_ref())
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
        home.write_credentials(br#"{"someOtherField":true}"#);
        assert_eq!(
            load_credentials(home.path(), store.as_ref(), account.as_ref())
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
        #[cfg(target_os = "macos")]
        assert_eq!(store.read_count(), 3);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(store.read_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn credentials_reject_insecure_permissions_symlinks_and_oversized_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let home = TestHome::new();
        let path = home.credential_path();
        let (store, account) = test_dependencies();
        let expected = super::unavailable_error();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            home.credentials("synthetic", Some("refresh"), Some(future_expiry()), "pro"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            load_credentials(home.path(), store.as_ref(), account.as_ref())
                .err()
                .unwrap(),
            expected
        );

        let target = home.path().join("target.json");
        fs::write(
            &target,
            home.credentials("synthetic", Some("refresh"), Some(future_expiry()), "pro"),
        )
        .unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        assert_eq!(
            load_credentials(home.path(), store.as_ref(), account.as_ref())
                .err()
                .unwrap(),
            expected
        );

        fs::remove_file(&path).unwrap();
        fs::write(&path, vec![b' '; MAX_CREDENTIAL_BYTES as usize + 1]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            load_credentials(home.path(), store.as_ref(), account.as_ref())
                .err()
                .unwrap(),
            expected
        );
        #[cfg(target_os = "macos")]
        assert_eq!(store.read_count(), 0);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(store.read_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_parent_and_relative_home_never_reach_account_resolution_or_store() {
        let home = TestHome::new();
        let claude_dir = home.path().join(".claude");
        fs::write(&claude_dir, b"not a directory").unwrap();
        let (store, _) = test_dependencies();
        let resolver = CountingAccountResolver::default();
        assert_eq!(
            load_credentials(home.path(), store.as_ref(), &resolver)
                .err()
                .unwrap(),
            super::unavailable_error()
        );
        assert_eq!(
            load_credentials(Path::new("relative-home"), store.as_ref(), &resolver)
                .err()
                .unwrap(),
            ErrorCode::ConfigurationError
        );
        assert_eq!(store.read_count(), 0);
        assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn invalid_file_contents_use_only_the_fixed_fake_store_fallback() {
        let home = TestHome::new();
        home.write_credentials(br#"{"claudeAiOauth":{"accessToken":""}}"#);
        let store = TestClaudeKeychain::with_record(home.credentials(
            "keychain-access",
            Some("keychain-refresh"),
            Some(future_expiry()),
            "pro",
        ));
        let resolver = FixedAccountResolver;
        let loaded = load_credentials(home.path(), &store, &resolver).unwrap();
        assert_eq!(loaded.access_token.as_str(), "keychain-access");
        assert_eq!(store.read_count(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn account_resolution_failure_stops_before_fake_store_access() {
        struct FailingResolver;
        impl super::ClaudeAccountResolver for FailingResolver {
            fn resolve(&self) -> Result<ClaudeAccount, ErrorCode> {
                Err(ErrorCode::CredentialsMissing)
            }
        }

        let home = TestHome::new();
        let store = TestClaudeKeychain::with_record(home.credentials(
            "keychain-access",
            Some("keychain-refresh"),
            Some(future_expiry()),
            "pro",
        ));
        assert_eq!(
            load_credentials(home.path(), &store, &FailingResolver)
                .err()
                .unwrap(),
            ErrorCode::CredentialsMissing
        );
        assert_eq!(store.read_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_credential_parents_never_reach_account_resolution_or_store() {
        use std::os::unix::fs::symlink;

        let home = TestHome::new();
        let claude_dir = home.path().join(".claude");
        let target = home.path().join("outside");
        fs::create_dir(&target).unwrap();
        let (store, _) = test_dependencies();
        let resolver = Arc::new(CountingAccountResolver::default());

        for destination in [target.clone(), home.path().join("dangling-target")] {
            symlink(&destination, &claude_dir).unwrap();
            assert_eq!(
                super::load_credentials(home.path(), store.as_ref(), resolver.as_ref())
                    .err()
                    .unwrap(),
                super::unavailable_error()
            );
            fs::remove_file(&claude_dir).unwrap();
        }

        assert_eq!(store.read_count(), 0);
        assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
    }

    #[derive(Default)]
    struct CountingAccountResolver(std::sync::atomic::AtomicUsize);

    impl super::ClaudeAccountResolver for CountingAccountResolver {
        fn resolve(&self) -> Result<ClaudeAccount, ErrorCode> {
            self.0.fetch_add(1, Ordering::SeqCst);
            ClaudeAccount::from_login_name("synthetic-login".to_owned())
                .ok_or(ErrorCode::CredentialsMissing)
        }
    }

    #[test]
    fn authorization_header_is_validated_and_marked_sensitive() {
        assert!(
            authorization_header("synthetic-token")
                .unwrap()
                .is_sensitive()
        );
        assert!(authorization_header("bad\r\nheader").is_none());
    }

    #[tokio::test]
    async fn usage_request_has_expected_headers_and_returns_retry_metadata() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "synthetic-access",
            None,
            Some(future_expiry()),
            "claude_pro",
        ));
        let (address, task) = server(vec![Reply::new(response(
            200,
            "Retry-After: 60\r\nContent-Type: application/json\r\n",
            br#"{"five_hour":{"utilization":20,"resets_at":"2027-01-15T13:00:00Z"}}"#,
        ))])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert!(outcome.result.is_ok());
        assert!(outcome.retry_after.is_some());
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.line, "GET /usage HTTP/1.1");
        assert_eq!(
            request.header("authorization"),
            Some("Bearer synthetic-access")
        );
        assert_eq!(request.header("anthropic-beta"), Some("oauth-2025-04-20"));
        assert_eq!(request.header("user-agent"), Some("QuotaBar"));
        assert_eq!(request.header("accept"), Some("application/json"));
        assert_eq!(request.header("content-type"), Some("application/json"));
    }

    #[tokio::test]
    async fn proactive_refresh_rotates_tokens_and_preserves_unknown_json_and_mode() {
        use std::os::unix::fs::PermissionsExt;

        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "old-access",
            Some("old-refresh"),
            Some(0),
            "claude_pro",
        ));
        let refresh_body =
            br#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#;
        let (address, task) = server(vec![
            Reply::new(response(
                200,
                "Content-Type: application/json\r\n",
                refresh_body,
            )),
            Reply::new(response(
                200,
                "Content-Type: application/json\r\n",
                br#"{"five_hour":{"utilization":10}}"#,
            )),
        ])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert!(outcome.result.is_ok());
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].line, "POST /refresh HTTP/1.1");
        let form: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], "old-refresh");
        assert_eq!(form["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(
            form["scope"],
            "user:profile user:inference user:sessions:claude_code"
        );
        assert_eq!(
            requests[1].header("authorization"),
            Some("Bearer new-access")
        );

        let stored: Value =
            serde_json::from_slice(&fs::read(home.credential_path()).unwrap()).unwrap();
        assert_eq!(stored["topUnknown"]["keep"], true);
        assert_eq!(stored["claudeAiOauth"]["nestedUnknown"]["keep"], "yes");
        assert_eq!(stored["claudeAiOauth"]["subscriptionType"], "claude_pro");
        assert_eq!(stored["claudeAiOauth"]["accessToken"], "new-access");
        assert_eq!(stored["claudeAiOauth"]["refreshToken"], "new-refresh");
        assert!(stored["claudeAiOauth"]["expiresAt"].as_i64().is_some());
        assert_eq!(
            fs::metadata(home.credential_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn usage_401_refreshes_once_then_retries_usage() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "old-access",
            Some("old-refresh"),
            Some(future_expiry()),
            "pro",
        ));
        let (address, task) = server(vec![
            Reply::new(response(401, "", b"ignored")),
            Reply::new(response(200, "", br#"{"access_token":"new-access","refresh_token":"next-refresh","expires_in":3600}"#)),
            Reply::new(response(200, "", br#"{"seven_day":{"utilization":40}}"#)),
        ])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert!(outcome.result.is_ok());
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].line, "GET /usage HTTP/1.1");
        assert_eq!(requests[1].line, "POST /refresh HTTP/1.1");
        assert_eq!(requests[2].line, "GET /usage HTTP/1.1");
        assert_eq!(
            requests[2].header("authorization"),
            Some("Bearer new-access")
        );
    }

    #[tokio::test]
    async fn proactively_refreshed_401_does_not_exceed_one_renewal() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "old-access",
            Some("old-refresh"),
            Some(0),
            "pro",
        ));
        let (address, task) = server(vec![
            Reply::new(response(
                200,
                "",
                br#"{"access_token":"new-access","expires_in":3600}"#,
            )),
            Reply::new(response(401, "", b"ignored")),
        ])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::LoginExpired);
        assert_eq!(task.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rejected_invalid_grant_is_login_expired_and_does_not_fetch_usage() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "old-access",
            Some("old-refresh"),
            Some(0),
            "pro",
        ));
        let (address, task) = server(vec![Reply::new(response(
            400,
            "Content-Type: application/json\r\n",
            br#"{"error":"invalid_grant","error_description":"private text"}"#,
        ))])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::LoginExpired);
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn usage_403_is_not_subscription_unavailable_and_does_not_refresh() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "access",
            Some("refresh"),
            Some(future_expiry()),
            "pro",
        ));
        let (address, task) = server(vec![Reply::new(response(403, "", b"private body"))]).await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn missing_refresh_token_does_not_call_oauth_after_401() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials("access", None, Some(future_expiry()), "pro"));
        let (address, task) = server(vec![Reply::new(response(401, "", b"ignored"))]).await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::LoginExpired);
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn future_retry_after_on_usage_401_prevents_oauth_request() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "access",
            Some("refresh"),
            Some(future_expiry()),
            "pro",
        ));
        let (address, task) = server(vec![Reply::new(response(
            401,
            "Retry-After: 90\r\n",
            b"ignored",
        ))])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::LoginExpired);
        assert!(outcome.retry_after.is_some());
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn future_retry_after_on_oauth_error_is_retained_without_usage() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials("access", Some("refresh"), Some(0), "pro"));
        let (address, task) = server(vec![Reply::new(response(
            429,
            "Retry-After: 120\r\n",
            b"ignored",
        ))])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::RateLimited);
        assert!(outcome.retry_after.is_some());
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn truncated_oauth_429_without_retry_after_is_terminal_without_usage() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials("old", Some("refresh"), Some(0), "pro"));
        let truncated = b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort".to_vec();
        let (address, task) = server(vec![Reply::new(truncated)]).await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::RateLimited);
        assert_eq!(outcome.retry_after, None);
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn oversized_oauth_429_with_unusable_retry_after_is_terminal_without_usage() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials("old", Some("refresh"), Some(0), "pro"));
        let oversized = format!(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: someday\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let (address, task) = server(vec![Reply::new(oversized)]).await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::RateLimited);
        assert_eq!(outcome.retry_after, None);
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn oauth_429_does_not_reload_or_use_concurrent_fresh_cli_credentials() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials("old", Some("old-refresh"), Some(0), "pro"));
        let path = home.credential_path();
        let replacement = home.credentials(
            "cli-access",
            Some("cli-refresh"),
            Some(future_expiry()),
            "claude_max",
        );
        let replacement_for_server = replacement.clone();
        let (address, task) = server(vec![Reply::new(response(429, "", b"ignored")).before(
            move || {
                fs::write(&path, replacement_for_server).unwrap();
                fs::set_permissions(&path, private_file_permissions()).unwrap();
            },
        )])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::RateLimited);
        assert_eq!(outcome.retry_after, None);
        assert_eq!(task.await.unwrap().len(), 1);
        assert_eq!(fs::read(home.credential_path()).unwrap(), replacement);
    }

    #[tokio::test]
    async fn future_retry_after_on_successful_oauth_persists_rotation_then_stops() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials("old", Some("old-refresh"), Some(0), "pro"));
        let (address, task) = server(vec![Reply::new(response(
            200,
            "Retry-After: 120\r\n",
            br#"{"access_token":"rotated","refresh_token":"next","expires_in":3600}"#,
        ))])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::RateLimited);
        assert!(outcome.retry_after.is_some());
        assert_eq!(task.await.unwrap().len(), 1);
        let stored: Value =
            serde_json::from_slice(&fs::read(home.credential_path()).unwrap()).unwrap();
        assert_eq!(stored["claudeAiOauth"]["accessToken"], "rotated");
        assert_eq!(stored["claudeAiOauth"]["refreshToken"], "next");
    }

    #[tokio::test]
    async fn body_failure_retains_retry_after_and_stops_followup_request() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "access",
            Some("refresh"),
            Some(future_expiry()),
            "pro",
        ));
        let truncated = b"HTTP/1.1 200 OK\r\nRetry-After: 120\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort".to_vec();
        let (address, task) = server(vec![Reply::new(truncated)]).await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::NetworkError);
        assert!(outcome.retry_after.is_some());
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn usage_error_retry_after_is_preserved_for_the_scheduler() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials("access", None, Some(future_expiry()), "pro"));
        let (address, task) = server(vec![Reply::new(response(
            503,
            "Retry-After: 60\r\n",
            b"ignored provider text",
        ))])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
        assert!(outcome.retry_after.is_some());
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cli_update_during_refresh_is_reloaded_not_overwritten_or_renewed() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials("old", Some("old-refresh"), Some(0), "pro"));
        let path = home.credential_path();
        let replacement = home.credentials(
            "cli-access",
            Some("cli-refresh"),
            Some(future_expiry()),
            "claude_max",
        );
        let replacement_for_server = replacement.clone();
        let (address, task) = server(vec![
            Reply::new(response(
                200,
                "",
                br#"{"access_token":"stale-rotated","refresh_token":"stale-refresh","expires_in":3600}"#,
            ))
            .before(move || {
                fs::write(&path, replacement_for_server).unwrap();
                fs::set_permissions(&path, private_file_permissions()).unwrap();
            }),
            Reply::new(response(200, "", br#"{"five_hour":{"utilization":25}}"#)),
        ])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert!(outcome.result.is_ok());
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].header("authorization"),
            Some("Bearer cli-access")
        );
        let stored = fs::read(home.credential_path()).unwrap();
        assert_eq!(stored, replacement);
    }

    #[tokio::test]
    async fn unsafe_concurrent_mode_change_prevents_using_rotated_response() {
        use std::os::unix::fs::PermissionsExt;

        let home = TestHome::new();
        home.write_credentials(&home.credentials("old", Some("old-refresh"), Some(0), "pro"));
        let path = home.credential_path();
        let (address, task) = server(vec![
            Reply::new(response(
                200,
                "",
                br#"{"access_token":"rotated","expires_in":3600}"#,
            ))
            .before(move || {
                fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
            }),
        ])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        #[cfg(target_os = "macos")]
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::KeychainBlocked);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::CredentialsMissing);
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn proactive_expiry_uses_the_five_minute_refresh_buffer() {
        let home = TestHome::new();
        let expires = Utc::now().timestamp_millis() + ChronoDuration::minutes(4).num_milliseconds();
        home.write_credentials(&home.credentials("old", Some("refresh"), Some(expires), "pro"));
        let (address, task) = server(vec![
            Reply::new(response(
                200,
                "",
                br#"{"access_token":"new","expires_in":3600}"#,
            )),
            Reply::new(response(200, "", b"{}")),
        ])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert!(outcome.result.is_ok());
        let requests = task.await.unwrap();
        assert_eq!(requests[0].line, "POST /refresh HTTP/1.1");
        assert_eq!(requests[1].line, "GET /usage HTTP/1.1");
    }

    #[tokio::test]
    async fn rejects_invalid_refresh_reply_without_persisting_or_using_old_token() {
        let home = TestHome::new();
        let original = home.credentials("old", Some("refresh"), Some(0), "pro");
        home.write_credentials(&original);
        let (address, task) = server(vec![Reply::new(response(
            200,
            "",
            br#"{"access_token":"","expires_in":3600}"#,
        ))])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
        assert_eq!(fs::read(home.credential_path()).unwrap(), original);
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn respects_one_refresh_maximum_after_a_rejected_401_retry() {
        let home = TestHome::new();
        home.write_credentials(&home.credentials(
            "old",
            Some("refresh"),
            Some(future_expiry()),
            "pro",
        ));
        let (address, task) = server(vec![
            Reply::new(response(401, "", b"ignored")),
            Reply::new(response(400, "", br#"{"error":"invalid_grant"}"#)),
        ])
        .await;

        let outcome = probe(&home, address).probe_at(now()).await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::LoginExpired);
        assert_eq!(task.await.unwrap().len(), 2);
    }

    #[test]
    fn file_size_limit_matches_shared_reader_contract() {
        assert_eq!(MAX_CREDENTIAL_BYTES, 1024 * 1024);
        assert_eq!(MAX_RESPONSE_BYTES, 256 * 1024);
        let decoded: OAuthReply =
            serde_json::from_slice(br#"{"access_token":"synthetic"}"#).unwrap();
        assert_eq!(decoded.access_token.as_deref(), Some("synthetic"));
    }
}
