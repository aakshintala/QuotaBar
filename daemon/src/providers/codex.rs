use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, StatusCode, Url};
use serde_json::Value;

use crate::feed::ErrorCode;
use crate::providers::credential_file::{
    JsonSnapshot, ReplaceOutcome, read_json_snapshot, replace_if_unchanged,
};
use crate::providers::http::{build_client, parse_retry_after, read_bounded_body};
use crate::quota::{Observation, Quota};
use crate::scheduler::AttemptOutcome;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const OAUTH_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const REFRESH_AGE: ChronoDuration = ChronoDuration::days(8);

/// A credential string which deliberately has no Debug or serialization API.
struct SecretString(String);

impl SecretString {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // NUL is valid UTF-8, so clearing the live allocation first is safe.
        unsafe { self.0.as_mut_vec().fill(0) };
        self.0.clear();
    }
}

/// Credentials and the exact source snapshot used for any conditional update.
/// This type intentionally does not implement Debug or Serialize.
struct AuthRecord {
    snapshot: JsonSnapshot,
    document: Value,
    access_token: SecretString,
    refresh_token: Option<SecretString>,
    account_id: Option<SecretString>,
    last_refresh: Option<String>,
}

impl Drop for AuthRecord {
    fn drop(&mut self) {
        wipe_json_strings(&mut self.document);
    }
}

#[cfg(test)]
type PostCommitHook = std::sync::Arc<dyn Fn(&Path) + Send + Sync>;

/// Codex usage probe. Production endpoints are fixed; tests alone can select
/// loopback HTTP endpoints. Construction does not read credentials or make a
/// request.
pub struct CodexProbe {
    home: PathBuf,
    client: Client,
    usage_url: Url,
    oauth_url: Url,
    #[cfg(test)]
    after_commit_hook: Option<PostCommitHook>,
}

impl CodexProbe {
    /// Build an isolated client with finite timeouts and no proxy, redirects,
    /// or automatic retries.
    pub fn new(home: PathBuf) -> Result<Self, ErrorCode> {
        let client = build_client(REQUEST_TIMEOUT, CONNECT_TIMEOUT)?;
        Ok(Self::with_client(home, &client))
    }

    /// Use a clone of the application's shared Reqwest client while keeping
    /// Codex's production endpoints fixed.
    pub(crate) fn with_client(home: PathBuf, client: &Client) -> Self {
        Self {
            home,
            client: client.clone(),
            usage_url: Url::parse(USAGE_URL).expect("fixed Codex usage URL is valid"),
            oauth_url: Url::parse(OAUTH_URL).expect("fixed OpenAI OAuth URL is valid"),
            #[cfg(test)]
            after_commit_hook: None,
        }
    }

    /// Load the explicit HOME-rooted CLI credential file, refresh where the
    /// existing Codex flow permits, and return the scheduler's common outcome.
    pub async fn probe(&self) -> AttemptOutcome {
        let mut auth = match load_auth(&self.home) {
            Ok(auth) => auth,
            Err(error) => return failure(error, None),
        };

        if needs_refresh(auth.last_refresh.as_deref(), Utc::now())
            && auth.refresh_token.is_some()
            && let Err(outcome) = self.refresh(&mut auth).await
        {
            let should_fall_back = outcome.retry_after.is_none()
                && outcome.result != Err(ErrorCode::LoginExpired)
                && outcome.result != Err(ErrorCode::PersistenceError);
            if !should_fall_back {
                return outcome;
            }
        }

        let usage = match self.fetch_usage(&auth).await {
            Ok(response) => response,
            Err(outcome) => return outcome,
        };

        if usage.status == StatusCode::UNAUTHORIZED {
            // A known server guard must not be bypassed by immediately using
            // the refresh endpoint, even if the rejected request was a 401.
            if usage.retry_after.is_some() || auth.refresh_token.is_none() {
                return failure(ErrorCode::LoginExpired, usage.retry_after);
            }
            if let Err(outcome) = self.refresh(&mut auth).await {
                return outcome;
            }

            // One auth-recovery attempt only. A second 401 is terminal; do not
            // create a refresh loop.
            let retry = match self.fetch_usage(&auth).await {
                Ok(response) => response,
                Err(outcome) => return outcome,
            };
            return self.finish_usage(retry);
        }

        self.finish_usage(usage)
    }

    async fn fetch_usage(&self, auth: &AuthRecord) -> Result<HttpResponse, AttemptOutcome> {
        let authorization = match authorization_header(&auth.access_token) {
            Some(value) => value,
            None => return Err(failure(ErrorCode::CredentialsMissing, None)),
        };
        let account = match auth.account_id.as_ref().map(account_header).transpose() {
            Ok(value) => value,
            Err(()) => return Err(failure(ErrorCode::CredentialsMissing, None)),
        };

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(USER_AGENT, HeaderValue::from_static("OpenUsage"));
        headers.insert(AUTHORIZATION, authorization);
        if let Some(account) = account {
            headers.insert("ChatGPT-Account-Id", account);
        }

        let response = match self
            .client
            .get(self.usage_url.clone())
            .headers(headers)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Err(failure(ErrorCode::NetworkError, None)),
        };
        Ok(collect_response(response).await)
    }

    fn finish_usage(&self, response: HttpResponse) -> AttemptOutcome {
        if response.status != StatusCode::OK {
            return failure(usage_status_error(response.status), response.retry_after);
        }
        let headers = response.headers.clone();
        let mut body = match response.body {
            Ok(body) => body,
            Err(error) => return failure(error, response.retry_after),
        };
        let parsed = parse_usage(&body, &headers, Utc::now());
        body.fill(0);
        match parsed {
            Ok(observation) => AttemptOutcome {
                result: Ok(observation),
                retry_after: response.retry_after,
            },
            Err(error) => failure(error, response.retry_after),
        }
    }

    async fn refresh(&self, auth: &mut AuthRecord) -> Result<(), AttemptOutcome> {
        let Some(refresh_token) = auth.refresh_token.as_ref() else {
            return Err(failure(ErrorCode::LoginExpired, None));
        };

        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token.expose()),
        ];
        let response = match self
            .client
            .post(self.oauth_url.clone())
            .form(&form)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Err(failure(ErrorCode::NetworkError, None)),
        };
        let response = collect_response(response).await;
        let status = response.status;
        let retry_after = response.retry_after;
        let mut body = match response.body {
            Ok(body) => body,
            Err(error) => {
                if status != StatusCode::OK {
                    return Err(failure(oauth_status_error(status), retry_after));
                }
                return Err(failure(error, retry_after));
            }
        };
        if status != StatusCode::OK {
            let recognized_rejection = is_rejected_refresh_token(&body);
            body.fill(0);
            let error = if recognized_rejection {
                ErrorCode::LoginExpired
            } else {
                oauth_status_error(status)
            };
            return Err(failure(error, retry_after));
        }

        let refresh = parse_refresh(&body);
        body.fill(0);
        let refresh = match refresh {
            Ok(refresh) => refresh,
            Err(error) => return Err(failure(error, retry_after)),
        };
        let last_refresh = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        if let Err(error) = apply_refresh_to_document(auth, &refresh, &last_refresh) {
            return Err(failure(error, retry_after));
        }

        let mut updated_bytes = match serde_json::to_vec(&auth.document) {
            Ok(bytes) => bytes,
            Err(_) => return Err(failure(ErrorCode::PersistenceError, retry_after)),
        };
        let outcome = replace_if_unchanged(
            &self.credential_path(),
            auth.snapshot.bytes(),
            &updated_bytes,
            auth.snapshot.mode(),
        );
        updated_bytes.fill(0);

        match outcome {
            Ok(ReplaceOutcome::SourceChanged) => {
                // The CLI won the compare. Never overwrite its newer file or
                // reuse/renew the rotated token from our stale snapshot.
                *auth = load_auth(&self.home).map_err(|error| failure(error, retry_after))?;
            }
            Ok(ReplaceOutcome::Replaced) => {
                self.reload_committed_record(auth)
                    .map_err(|error| failure(error, retry_after))?;
            }
            Ok(ReplaceOutcome::CommittedDurabilityUncertain) => {
                // Rename is the commit point. Reload the complete committed
                // record and never retry tokens from the pre-rename version.
                eprintln!(
                    "warning: Codex credentials committed; directory durability is unconfirmed"
                );
                self.reload_committed_record(auth)
                    .map_err(|error| failure(error, retry_after))?;
            }
            Err(_) => return Err(failure(ErrorCode::PersistenceError, retry_after)),
        }

        // A valid future Retry-After applies even to a successful refresh. The
        // rotation is already persisted/reloaded, but no usage request follows.
        if retry_after.is_some() {
            return Err(failure(ErrorCode::RateLimited, retry_after));
        }
        Ok(())
    }

    fn reload_committed_record(&self, auth: &mut AuthRecord) -> Result<(), ErrorCode> {
        #[cfg(test)]
        if let Some(hook) = &self.after_commit_hook {
            hook(&self.credential_path());
        }
        *auth = load_auth(&self.home).map_err(|_| ErrorCode::PersistenceError)?;
        Ok(())
    }

    fn credential_path(&self) -> PathBuf {
        self.home.join(".codex/auth.json")
    }

    #[cfg(test)]
    fn for_loopback_test(home: PathBuf, usage_url: Url, oauth_url: Url, timeout: Duration) -> Self {
        assert_eq!(usage_url.scheme(), "http");
        assert_eq!(usage_url.host_str(), Some("127.0.0.1"));
        assert_eq!(oauth_url.scheme(), "http");
        assert_eq!(oauth_url.host_str(), Some("127.0.0.1"));
        Self {
            home,
            client: build_client(timeout, timeout).expect("test HTTP client builds"),
            usage_url,
            oauth_url,
            after_commit_hook: None,
        }
    }
}

struct HttpResponse {
    status: StatusCode,
    retry_after: Option<DateTime<Utc>>,
    body: Result<Vec<u8>, ErrorCode>,
    headers: HeaderMap,
}

async fn collect_response(response: reqwest::Response) -> HttpResponse {
    // Reqwest resolves `send` at the headers boundary. Anchor Retry-After here,
    // before reading a potentially slow or incomplete body.
    let received_at = Utc::now();
    let status = response.status();
    let retry_after = parse_retry_after(response.headers().get("retry-after"), received_at);
    let headers = response.headers().clone();
    let body = read_bounded_body(response, MAX_RESPONSE_BYTES).await;
    HttpResponse {
        status,
        retry_after,
        body,
        headers,
    }
}

struct RefreshTokens {
    access_token: SecretString,
    refresh_token: Option<SecretString>,
    id_token: Option<SecretString>,
}

fn load_auth(home: &Path) -> Result<AuthRecord, ErrorCode> {
    let snapshot = read_json_snapshot(&home.join(".codex/auth.json"), MAX_CREDENTIAL_BYTES)
        .map_err(|_| ErrorCode::CredentialsMissing)?;
    let mut document: Value =
        serde_json::from_slice(snapshot.bytes()).map_err(|_| ErrorCode::CredentialsMissing)?;
    let Some(tokens) = document.get("tokens").and_then(Value::as_object) else {
        wipe_json_strings(&mut document);
        return Err(ErrorCode::CredentialsMissing);
    };
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| valid_access_token(value))
        .map(str::to_owned);
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| SecretString::new(value.to_owned()));
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(|value| SecretString::new(value.to_owned()));
    let last_refresh = document
        .get("last_refresh")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(access_token) = access_token else {
        wipe_json_strings(&mut document);
        return Err(ErrorCode::CredentialsMissing);
    };

    Ok(AuthRecord {
        snapshot,
        document,
        access_token: SecretString::new(access_token),
        refresh_token,
        account_id,
        last_refresh,
    })
}

fn needs_refresh(last_refresh: Option<&str>, now: DateTime<Utc>) -> bool {
    let Some(last_refresh) = last_refresh else {
        return true;
    };
    let Ok(last_refresh) = DateTime::parse_from_rfc3339(last_refresh) else {
        return true;
    };
    now.signed_duration_since(last_refresh.with_timezone(&Utc)) > REFRESH_AGE
}

fn valid_access_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !byte.is_ascii_whitespace())
}

fn authorization_header(token: &SecretString) -> Option<HeaderValue> {
    if !valid_access_token(token.expose()) {
        return None;
    }
    let value = SecretString::new(format!("Bearer {}", token.expose()));
    let mut header = HeaderValue::from_str(value.expose()).ok()?;
    header.set_sensitive(true);
    Some(header)
}

fn account_header(account_id: &SecretString) -> Result<HeaderValue, ()> {
    let mut header = HeaderValue::from_str(account_id.expose()).map_err(|_| ())?;
    header.set_sensitive(true);
    Ok(header)
}

fn parse_refresh(body: &[u8]) -> Result<RefreshTokens, ErrorCode> {
    let mut value: Value = serde_json::from_slice(body).map_err(|_| ErrorCode::ProviderError)?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| valid_access_token(value))
        .map(|value| SecretString::new(value.to_owned()));
    let refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| SecretString::new(value.to_owned()));
    let id_token = value
        .get("id_token")
        .and_then(Value::as_str)
        .map(|value| SecretString::new(value.to_owned()));
    wipe_json_strings(&mut value);
    let access_token = access_token.ok_or(ErrorCode::ProviderError)?;
    Ok(RefreshTokens {
        access_token,
        refresh_token,
        id_token,
    })
}

fn apply_refresh_to_document(
    auth: &mut AuthRecord,
    tokens: &RefreshTokens,
    last_refresh: &str,
) -> Result<(), ErrorCode> {
    let document = auth
        .document
        .as_object_mut()
        .ok_or(ErrorCode::PersistenceError)?;
    let token_values = document
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or(ErrorCode::PersistenceError)?;
    token_values.insert(
        "access_token".to_owned(),
        Value::String(tokens.access_token.expose().to_owned()),
    );
    if let Some(refresh_token) = &tokens.refresh_token {
        token_values.insert(
            "refresh_token".to_owned(),
            Value::String(refresh_token.expose().to_owned()),
        );
    }
    if let Some(id_token) = &tokens.id_token {
        token_values.insert(
            "id_token".to_owned(),
            Value::String(id_token.expose().to_owned()),
        );
    }
    document.insert(
        "last_refresh".to_owned(),
        Value::String(last_refresh.to_owned()),
    );
    auth.last_refresh = Some(last_refresh.to_owned());
    Ok(())
}

fn is_rejected_refresh_token(body: &[u8]) -> bool {
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let code = value
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| value.get("code").and_then(Value::as_str));
    let rejected = matches!(
        code,
        Some("refresh_token_expired" | "refresh_token_reused" | "refresh_token_invalidated")
    );
    wipe_json_strings(&mut value);
    rejected
}

fn usage_status_error(status: StatusCode) -> ErrorCode {
    match status {
        StatusCode::UNAUTHORIZED => ErrorCode::LoginExpired,
        StatusCode::FORBIDDEN => ErrorCode::ProviderError,
        StatusCode::TOO_MANY_REQUESTS => ErrorCode::RateLimited,
        _ => ErrorCode::ProviderError,
    }
}

fn oauth_status_error(status: StatusCode) -> ErrorCode {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED => ErrorCode::LoginExpired,
        StatusCode::FORBIDDEN => ErrorCode::ProviderError,
        StatusCode::TOO_MANY_REQUESTS => ErrorCode::RateLimited,
        _ => ErrorCode::ProviderError,
    }
}

fn failure(error: ErrorCode, retry_after: Option<DateTime<Utc>>) -> AttemptOutcome {
    AttemptOutcome {
        result: Err(error),
        retry_after,
    }
}

fn parse_usage(
    body: &[u8],
    headers: &HeaderMap,
    captured_at: DateTime<Utc>,
) -> Result<Observation, ErrorCode> {
    let value: Value = serde_json::from_slice(body).map_err(|_| ErrorCode::ProviderError)?;
    let root = value.as_object().ok_or(ErrorCode::ProviderError)?;
    let rate_limit = root.get("rate_limit").and_then(Value::as_object);
    let mut quotas = Vec::with_capacity(3);

    for (name, key, label, duration, header_name) in [
        (
            "primary",
            "session",
            "Session",
            5 * 60 * 60,
            "x-codex-primary-used-percent",
        ),
        (
            "secondary",
            "weekly",
            "Weekly",
            7 * 24 * 60 * 60,
            "x-codex-secondary-used-percent",
        ),
    ] {
        let window = rate_limit
            .and_then(|rate| rate.get(&format!("{name}_window")))
            .and_then(Value::as_object);
        let used = parse_header_number(headers, header_name).or_else(|| {
            window
                .and_then(|window| window.get("used_percent"))
                .and_then(finite_number)
        });
        let Some(used) = used else {
            continue;
        };
        let resets_at = parse_reset(window, captured_at)?;
        quotas.push(Quota {
            key: key.to_owned(),
            label: label.to_owned(),
            percent_remaining: Some((100.0 - used).clamp(0.0, 100.0)),
            balance_remaining: None,
            balance_used: None,
            balance_cap: None,
            balance_unit: None,
            units_used: None,
            units_limit: None,
            resets_at,
            window_duration_seconds: Some(duration),
        });
    }

    let credits = root.get("credits").and_then(Value::as_object);
    let has_credits = credits
        .and_then(|credits| credits.get("has_credits"))
        .and_then(Value::as_bool);
    if has_credits != Some(false)
        && let Some(balance) =
            parse_header_number(headers, "x-codex-credits-balance").or_else(|| {
                credits
                    .and_then(|credits| credits.get("balance"))
                    .and_then(balance_number)
            })
        && balance > 0.0
    {
        quotas.push(Quota {
            key: "time:Credits".to_owned(),
            label: "Credits".to_owned(),
            percent_remaining: None,
            balance_remaining: Some(balance),
            balance_used: None,
            balance_cap: None,
            balance_unit: Some("credits".to_owned()),
            units_used: None,
            units_limit: None,
            resets_at: None,
            window_duration_seconds: None,
        });
    }

    let tier = root
        .get("plan_type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_uppercase);

    Ok(Observation {
        captured_at,
        tier,
        quotas,
    })
}

fn parse_header_number(headers: &HeaderMap, name: &'static str) -> Option<f64> {
    let value = headers.get(name)?.to_str().ok()?.parse::<f64>().ok()?;
    value.is_finite().then_some(value)
}

fn finite_number(value: &Value) -> Option<f64> {
    let value = value.as_number()?.as_f64()?;
    value.is_finite().then_some(value)
}

fn balance_number(value: &Value) -> Option<f64> {
    let value = match value {
        Value::String(value) => value.parse::<f64>().ok()?,
        Value::Number(value) => value.as_f64()?,
        _ => return None,
    };
    value.is_finite().then_some(value)
}

fn parse_reset(
    window: Option<&serde_json::Map<String, Value>>,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, ErrorCode> {
    let Some(window) = window else {
        return Ok(None);
    };
    if let Some(reset_at) = window.get("reset_at")
        && let Some(seconds) = finite_number(reset_at)
    {
        return unix_seconds(seconds)
            .map(Some)
            .ok_or(ErrorCode::ProviderError);
    }
    if let Some(reset_after) = window.get("reset_after_seconds")
        && let Some(seconds) = finite_number(reset_after)
    {
        let now_seconds =
            now.timestamp() as f64 + f64::from(now.timestamp_subsec_nanos()) / 1_000_000_000.0;
        return unix_seconds(now_seconds + seconds)
            .map(Some)
            .ok_or(ErrorCode::ProviderError);
    }
    Ok(None)
}

fn unix_seconds(value: f64) -> Option<DateTime<Utc>> {
    if !value.is_finite() {
        return None;
    }
    let whole_seconds = value.floor();
    // The upper bound is exclusive because i64::MAX rounds to 2^63 as f64.
    if whole_seconds < i64::MIN as f64 || whole_seconds >= -(i64::MIN as f64) {
        return None;
    }
    let mut seconds = whole_seconds as i64;
    let mut nanos = ((value - whole_seconds) * 1_000_000_000.0).round() as u32;
    if nanos == 1_000_000_000 {
        seconds = seconds.checked_add(1)?;
        nanos = 0;
    }
    DateTime::from_timestamp(seconds, nanos)
}

fn wipe_json_strings(value: &mut Value) {
    match value {
        Value::String(string) => {
            // NUL is valid UTF-8, so replacing the live string contents is safe.
            unsafe { string.as_mut_vec().fill(0) };
            string.clear();
        }
        Value::Array(values) => values.iter_mut().for_each(wipe_json_strings),
        Value::Object(values) => values.values_mut().for_each(wipe_json_strings),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CodexProbe, MAX_RESPONSE_BYTES, SecretString, load_auth, needs_refresh, parse_usage,
    };
    use crate::feed::ErrorCode;
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    use reqwest::Url;
    use reqwest::header::{HeaderMap, HeaderValue};
    use serde_json::Value;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Self {
            let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("quotabard-codex-{}-{sequence}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn credential_path(&self) -> PathBuf {
            self.path().join(".codex/auth.json")
        }

        fn write_auth(&self, bytes: &[u8], mode: u32) {
            let path = self.credential_path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }

        fn write_current_auth(&self, access: &str, refresh: Option<&str>) {
            let last_refresh = (Utc::now() - ChronoDuration::days(1)).to_rfc3339();
            let mut tokens = serde_json::Map::new();
            tokens.insert("access_token".into(), access.into());
            if let Some(refresh) = refresh {
                tokens.insert("refresh_token".into(), refresh.into());
            }
            tokens.insert("account_id".into(), "synthetic-account".into());
            let value = serde_json::json!({"tokens": tokens, "last_refresh": last_refresh});
            self.write_auth(&serde_json::to_vec(&value).unwrap(), 0o600);
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Request {
        first_line: String,
        headers: std::collections::HashMap<String, String>,
        body: Vec<u8>,
        stream: TcpStream,
    }

    fn server() -> (TcpListener, Url) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = TcpListener::from_std(listener).unwrap();
        let address = listener.local_addr().unwrap();
        let usage = Url::parse(&format!("http://{address}/usage")).unwrap();
        (listener, usage)
    }

    fn probe(home: &TestHome, usage_url: Url, oauth_url: Url) -> CodexProbe {
        CodexProbe::for_loopback_test(
            home.path().to_path_buf(),
            usage_url,
            oauth_url,
            Duration::from_secs(2),
        )
    }

    async fn read_request(stream: TcpStream) -> Request {
        let mut reader = BufReader::new(stream);
        let mut first_line = String::new();
        reader.read_line(&mut first_line).await.unwrap();
        let mut headers = std::collections::HashMap::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
            }
        }
        let length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await.unwrap();
        Request {
            first_line,
            headers,
            body,
            stream: reader.into_inner(),
        }
    }

    async fn send_response(mut stream: TcpStream, status: u16, headers: &str, body: &[u8]) {
        let phrase = if status == 200 { "OK" } else { "Synthetic" };
        let mut response = format!(
            "HTTP/1.1 {status} {phrase}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        stream.write_all(&response).await.unwrap();
    }

    fn usage_json() -> &'static [u8] {
        br#"{"rate_limit":{"primary_window":{"used_percent":10,"reset_at":1800003600},"secondary_window":{"used_percent":20,"reset_after_seconds":604800}},"plan_type":"plus"}"#
    }

    fn auth_with(access: &str, refresh: &str, last_refresh: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "tokens": {"access_token": access, "refresh_token": refresh, "account_id": "synthetic-account", "id_token": "synthetic-id", "unknown_nested": {"preserve": true}},
            "last_refresh": last_refresh,
            "unknown_root": {"preserve": "yes"}
        }))
        .unwrap()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    #[tokio::test]
    async fn constructor_does_not_read_credentials_and_api_key_auth_is_unsupported() {
        let home = TestHome::new();
        let (_listener, usage) = server();
        let oauth = Url::parse(&format!("http://{}/oauth", usage.authority())).unwrap();
        let probe = CodexProbe::new(home.path().to_path_buf()).unwrap();
        drop(probe);
        let outcome = CodexProbe::for_loopback_test(
            home.path().to_path_buf(),
            usage,
            oauth,
            Duration::from_secs(2),
        )
        .probe()
        .await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::CredentialsMissing);

        home.write_auth(br#"{"OPENAI_API_KEY":"synthetic-api-key"}"#, 0o600);
        assert!(matches!(
            load_auth(home.path()),
            Err(ErrorCode::CredentialsMissing)
        ));
    }

    #[test]
    fn needs_refresh_matches_eight_day_age_rule_and_invalid_dates() {
        let now = now();
        let recent = (now - ChronoDuration::days(8)).to_rfc3339();
        let old = (now - ChronoDuration::days(9)).to_rfc3339();
        let future = (now + ChronoDuration::days(1)).to_rfc3339();
        assert!(!needs_refresh(Some(&recent), now));
        assert!(needs_refresh(Some(&old), now));
        assert!(!needs_refresh(Some(&future), now));
        assert!(needs_refresh(None, now));
        assert!(needs_refresh(Some("not-a-date"), now));
    }

    #[test]
    fn retry_after_parser_rejects_unrepresentably_long_delays() {
        let value = HeaderValue::from_static("18446744073709551615");
        assert!(crate::providers::http::parse_retry_after(Some(&value), now()).is_none());
    }

    #[test]
    fn parses_header_precedence_fallback_resets_credits_and_tier() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-primary-used-percent",
            HeaderValue::from_static("25.5"),
        );
        headers.insert(
            "x-codex-secondary-used-percent",
            HeaderValue::from_static("bad"),
        );
        headers.insert("x-codex-credits-balance", HeaderValue::from_static("750.0"));
        let body = br#"{
            "rate_limit": {
                "primary_window": {"used_percent": 80, "reset_at": 1800003600.5},
                "secondary_window": {"used_percent": 60, "reset_after_seconds": 432000}
            },
            "credits": {"has_credits": true, "balance": "1"},
            "plan_type": "plus"
        }"#;

        let observation = parse_usage(body, &headers, now()).unwrap();
        assert_eq!(observation.tier.as_deref(), Some("PLUS"));
        assert_eq!(observation.quotas.len(), 3);
        assert_eq!(observation.quotas[0].percent_remaining, Some(74.5));
        assert_eq!(observation.quotas[0].key, "session");
        let reset_at = observation.quotas[0].resets_at.as_ref().unwrap();
        assert_eq!(reset_at.timestamp(), 1_800_003_600);
        assert_eq!(reset_at.timestamp_subsec_nanos(), 500_000_000);
        assert_eq!(observation.quotas[1].percent_remaining, Some(40.0));
        assert_eq!(
            observation.quotas[1].resets_at,
            Some(now() + ChronoDuration::seconds(432_000))
        );
        assert_eq!(observation.quotas[2].key, "time:Credits");
        assert_eq!(observation.quotas[2].balance_remaining, Some(750.0));
        assert_eq!(observation.quotas[2].balance_cap, None);
        assert_eq!(
            observation.quotas[2].balance_unit.as_deref(),
            Some("credits")
        );
    }

    #[test]
    fn clamps_used_percent_and_omits_zero_or_explicitly_absent_credits() {
        let headers = HeaderMap::new();
        let high_used = br#"{"rate_limit":{"primary_window":{"used_percent":140},"secondary_window":{"used_percent":-20}},"credits":{"has_credits":false,"balance":"500"}}"#;
        let observation = parse_usage(high_used, &headers, now()).unwrap();
        assert_eq!(observation.quotas[0].percent_remaining, Some(0.0));
        assert_eq!(observation.quotas[1].percent_remaining, Some(100.0));
        assert_eq!(observation.quotas.len(), 2);

        let zero = br#"{"credits":{"balance":0}}"#;
        assert!(
            parse_usage(zero, &headers, now())
                .unwrap()
                .quotas
                .is_empty()
        );
        let string_balance = br#"{"credits":{"balance":"1234.5"}}"#;
        let credits = parse_usage(string_balance, &headers, now()).unwrap();
        assert_eq!(credits.quotas[0].balance_remaining, Some(1234.5));
        assert_eq!(credits.quotas[0].balance_cap, None);
    }

    #[test]
    fn malformed_json_and_invalid_reset_timestamps_are_rejected_safely() {
        let headers = HeaderMap::new();
        assert!(parse_usage(b"not-json", &headers, now()).is_err());
        assert!(
            parse_usage(
                br#"{"rate_limit":{"primary_window":{"used_percent":1,"reset_at":1e999}}}"#,
                &headers,
                now()
            )
            .is_err()
        );
        assert!(parse_usage(br#"{"rate_limit":{"primary_window":{"used_percent":1,"reset_at":999999999999999999999999}}}"#, &headers, now()).is_err());
        assert!(parse_usage(br#"{"rate_limit":{"primary_window":{"used_percent":1,"reset_after_seconds":1e999}}}"#, &headers, now()).is_err());
        assert!(
            parse_usage(
                br#"{"rate_limit":{"primary_window":{"used_percent":1e999}}}"#,
                &headers,
                now()
            )
            .is_err()
        );
        assert!(parse_usage(br#"[]"#, &headers, now()).is_err());
    }

    #[tokio::test]
    async fn proactive_refresh_form_encodes_delimiters_preserves_json_and_file_mode() {
        let home = TestHome::new();
        let old = auth_with(
            "old-access",
            "old+refresh/&= space",
            &(Utc::now() - ChronoDuration::days(9)).to_rfc3339(),
        );
        home.write_auth(&old, 0o600);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            assert!(request.first_line.starts_with("POST /oauth "));
            assert_eq!(
                request.headers.get("content-type").map(String::as_str),
                Some("application/x-www-form-urlencoded")
            );
            let form = String::from_utf8(request.body).unwrap();
            assert!(form.contains("grant_type=refresh_token"));
            assert!(form.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
            assert!(form.contains("refresh_token=old%2Brefresh%2F%26%3D+space"));
            send_response(request.stream, 200, "Content-Type: application/json\r\n", br#"{"access_token":"new-access","refresh_token":"new-refresh","id_token":"new-id"}"#).await;

            let request = read_request(listener.accept().await.unwrap().0).await;
            assert!(request.first_line.starts_with("GET /usage "));
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer new-access")
            );
            assert_eq!(
                request
                    .headers
                    .get("chatgpt-account-id")
                    .map(String::as_str),
                Some("synthetic-account")
            );
            assert_eq!(
                request.headers.get("user-agent").map(String::as_str),
                Some("OpenUsage")
            );
            send_response(
                request.stream,
                200,
                "Content-Type: application/json\r\n",
                usage_json(),
            )
            .await;
        });

        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert!(outcome.result.is_ok());
        assert_eq!(outcome.result.unwrap().tier.as_deref(), Some("PLUS"));
        task.await.unwrap();

        let saved: Value =
            serde_json::from_slice(&fs::read(home.credential_path()).unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "new-access");
        assert_eq!(saved["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(saved["tokens"]["id_token"], "new-id");
        assert_eq!(saved["tokens"]["unknown_nested"]["preserve"], true);
        assert_eq!(saved["unknown_root"]["preserve"], "yes");
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
    async fn proactive_refresh_failure_uses_existing_token_unless_login_is_expired() {
        let home = TestHome::new();
        home.write_auth(
            &auth_with("existing-access", "refresh-token", "bad-date"),
            0o600,
        );
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            assert!(request.first_line.starts_with("POST /oauth "));
            send_response(request.stream, 503, "", b"private upstream message").await;
            let request = read_request(listener.accept().await.unwrap().0).await;
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer existing-access")
            );
            send_response(request.stream, 200, "", usage_json()).await;
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert!(outcome.result.is_ok());
        task.await.unwrap();

        let home = TestHome::new();
        home.write_auth(&auth_with("old", "refresh-token", "bad-date"), 0o600);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            send_response(
                request.stream,
                400,
                "",
                br#"{"error":{"code":"refresh_token_reused"}}"#,
            )
            .await;
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::LoginExpired);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn usage_401_with_future_retry_after_does_not_bypass_guard_via_oauth() {
        let home = TestHome::new();
        home.write_current_auth("rejected-access", Some("refresh-token"));
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            assert!(request.first_line.starts_with("GET /usage "));
            send_response(request.stream, 401, "Retry-After: 90\r\n", b"ignored").await;
            assert!(
                timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::LoginExpired);
        let retry_after = outcome.retry_after.unwrap();
        assert!(retry_after > Utc::now());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn successful_oauth_retry_after_persists_rotation_then_skips_usage() {
        let home = TestHome::new();
        home.write_auth(&auth_with("old-access", "old-refresh", "bad-date"), 0o600);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            assert!(request.first_line.starts_with("POST /oauth "));
            send_response(
                request.stream,
                200,
                "Retry-After: 60\r\n",
                br#"{"access_token":"rotated-access","refresh_token":"rotated-refresh"}"#,
            )
            .await;
            assert!(
                timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });

        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::RateLimited);
        assert!(outcome.retry_after.unwrap() > Utc::now());
        task.await.unwrap();
        let saved: Value =
            serde_json::from_slice(&fs::read(home.credential_path()).unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "rotated-access");
        assert_eq!(saved["tokens"]["refresh_token"], "rotated-refresh");
    }

    #[tokio::test]
    async fn usage_401_refreshes_once_and_retries_with_rotated_access_token() {
        let home = TestHome::new();
        home.write_current_auth("rejected-access", Some("refresh-token"));
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            assert!(request.first_line.starts_with("GET /usage "));
            send_response(request.stream, 401, "", b"ignored").await;

            let request = read_request(listener.accept().await.unwrap().0).await;
            assert!(request.first_line.starts_with("POST /oauth "));
            send_response(
                request.stream,
                200,
                "",
                br#"{"access_token":"recovered-access","refresh_token":"recovered-refresh"}"#,
            )
            .await;

            let request = read_request(listener.accept().await.unwrap().0).await;
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer recovered-access")
            );
            send_response(request.stream, 200, "", usage_json()).await;
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert!(outcome.result.is_ok());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn oauth_retry_after_http_date_on_error_stops_proactive_fallback() {
        let home = TestHome::new();
        home.write_auth(&auth_with("old-access", "old-refresh", "bad-date"), 0o600);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            let date =
                httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(120));
            send_response(
                request.stream,
                429,
                &format!("Retry-After: {date}\r\n"),
                b"ignored",
            )
            .await;
            assert!(
                timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::RateLimited);
        assert!(outcome.retry_after.unwrap() > Utc::now());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn retry_after_delta_starts_at_headers_not_after_a_slow_body() {
        let home = TestHome::new();
        home.write_current_auth("access", None);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            let mut stream = request.stream;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nRetry-After: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(1_300)).await;
            stream.write_all(b"{}").await.unwrap();
        });
        let probe = CodexProbe::for_loopback_test(
            home.path().to_path_buf(),
            usage_url,
            oauth_url,
            Duration::from_secs(4),
        );
        let outcome = probe.probe().await;
        assert!(outcome.result.is_ok());
        assert!(outcome.retry_after.unwrap() < Utc::now());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn retry_after_survives_truncated_usage_body() {
        let home = TestHome::new();
        home.write_current_auth("access", None);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            let response = b"HTTP/1.1 200 OK\r\nRetry-After: 45\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial";
            let mut stream = request.stream;
            stream.write_all(response).await.unwrap();
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::NetworkError);
        assert!(outcome.retry_after.unwrap() > Utc::now());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn retry_after_is_retained_for_valid_and_unparseable_success_bodies() {
        for (body, should_succeed) in [(usage_json(), true), (b"not-json".as_slice(), false)] {
            let home = TestHome::new();
            home.write_current_auth("access", None);
            let (listener, usage_url) = server();
            let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
            let task = tokio::spawn(async move {
                let request = read_request(listener.accept().await.unwrap().0).await;
                send_response(request.stream, 200, "Retry-After: 75\r\n", body).await;
            });
            let outcome = probe(&home, usage_url, oauth_url).probe().await;
            assert_eq!(outcome.result.is_ok(), should_succeed);
            if !should_succeed {
                assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
            }
            assert!(outcome.retry_after.unwrap() > Utc::now());
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn oauth_success_parse_failure_keeps_retry_after_and_skips_usage() {
        let home = TestHome::new();
        home.write_auth(&auth_with("old-access", "old-refresh", "bad-date"), 0o600);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            send_response(request.stream, 200, "Retry-After: 60\r\n", b"not-json").await;
            assert!(
                timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
        assert!(outcome.retry_after.unwrap() > Utc::now());
        task.await.unwrap();
        let saved: Value =
            serde_json::from_slice(&fs::read(home.credential_path()).unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "old-access");
    }

    #[tokio::test]
    async fn cli_change_during_refresh_reloads_and_uses_new_file_without_overwriting() {
        let home = TestHome::new();
        home.write_auth(&auth_with("old-access", "old-refresh", "bad-date"), 0o600);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let cli_auth = auth_with("cli-access", "cli-refresh", "2026-06-10T00:00:00Z");
        let path = home.credential_path();
        let expected_auth = cli_auth.clone();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            fs::write(&path, &cli_auth).unwrap();
            send_response(request.stream, 200, "", br#"{"access_token":"stale-rotated-access","refresh_token":"stale-rotated-refresh"}"#).await;
            let request = read_request(listener.accept().await.unwrap().0).await;
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer cli-access")
            );
            send_response(request.stream, 200, "", usage_json()).await;
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert!(outcome.result.is_ok());
        task.await.unwrap();
        assert_eq!(fs::read(home.credential_path()).unwrap(), expected_auth);
    }

    #[tokio::test]
    async fn postcommit_cli_change_reloads_complete_record_before_later_rotation() {
        let home = TestHome::new();
        home.write_auth(&auth_with("old-access", "old-refresh", "bad-date"), 0o600);

        let cli_auth = serde_json::to_vec(&serde_json::json!({
            "tokens": {
                "access_token": "cli-access-c",
                "refresh_token": "cli-refresh-c",
                "account_id": "cli-account-c",
                "id_token": "cli-id-c",
                "cli_nested": {"version": "C"}
            },
            "last_refresh": (Utc::now() - ChronoDuration::days(1)).to_rfc3339(),
            "cli_root": {"version": "C"}
        }))
        .unwrap();
        let path = home.credential_path();
        let changed = std::sync::Arc::new(AtomicBool::new(false));
        let hook_changed = std::sync::Arc::clone(&changed);
        let hook_auth = cli_auth.clone();

        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let server_task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            assert!(request.first_line.starts_with("POST /oauth "));
            send_response(
                request.stream,
                200,
                "",
                br#"{"access_token":"rotated-access-b","refresh_token":"rotated-refresh-b"}"#,
            )
            .await;

            let request = read_request(listener.accept().await.unwrap().0).await;
            let first_authorization = request.headers["authorization"].clone();
            send_response(request.stream, 401, "", b"expired").await;

            let request = read_request(listener.accept().await.unwrap().0).await;
            let refresh_form = String::from_utf8(request.body).unwrap();
            send_response(
                request.stream,
                200,
                "",
                br#"{"access_token":"renewed-access-d","refresh_token":"renewed-refresh-d"}"#,
            )
            .await;

            let request = read_request(listener.accept().await.unwrap().0).await;
            let retry_authorization = request.headers["authorization"].clone();
            send_response(request.stream, 200, "", usage_json()).await;
            (first_authorization, refresh_form, retry_authorization)
        });

        let mut codex = probe(&home, usage_url, oauth_url);
        codex.after_commit_hook = Some(std::sync::Arc::new(move |credential_path| {
            if !hook_changed.swap(true, Ordering::SeqCst) {
                fs::write(credential_path, &hook_auth).unwrap();
            }
        }));

        let outcome = codex.probe().await;
        assert!(outcome.result.is_ok());
        let (first_authorization, refresh_form, retry_authorization) = server_task.await.unwrap();
        assert_eq!(first_authorization, "Bearer cli-access-c");
        assert!(refresh_form.contains("refresh_token=cli-refresh-c"));
        assert_eq!(retry_authorization, "Bearer renewed-access-d");
        assert!(changed.load(Ordering::SeqCst));

        let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "renewed-access-d");
        assert_eq!(saved["tokens"]["refresh_token"], "renewed-refresh-d");
        assert_eq!(saved["tokens"]["account_id"], "cli-account-c");
        assert_eq!(saved["tokens"]["id_token"], "cli-id-c");
        assert_eq!(saved["tokens"]["cli_nested"]["version"], "C");
        assert_eq!(saved["cli_root"]["version"], "C");
    }

    #[tokio::test]
    async fn precommit_file_failure_returns_persistence_error_without_usage_request() {
        let home = TestHome::new();
        home.write_auth(&auth_with("old-access", "old-refresh", "bad-date"), 0o600);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let codex_directory = home.path().join(".codex");
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            fs::remove_dir_all(&codex_directory).unwrap();
            send_response(
                request.stream,
                200,
                "",
                br#"{"access_token":"rotated-access","refresh_token":"rotated-refresh"}"#,
            )
            .await;
            assert!(
                timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::PersistenceError);
        task.await.unwrap();
        assert!(!home.credential_path().exists());
    }

    #[tokio::test]
    async fn maps_403_as_provider_error_and_429_retry_after() {
        for (status, expected, retry) in [
            (403, ErrorCode::ProviderError, ""),
            (429, ErrorCode::RateLimited, "Retry-After: 30\r\n"),
        ] {
            let home = TestHome::new();
            home.write_current_auth("access", None);
            let (listener, usage_url) = server();
            let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
            let task = tokio::spawn(async move {
                let request = read_request(listener.accept().await.unwrap().0).await;
                send_response(request.stream, status, retry, b"sensitive response text").await;
            });
            let outcome = probe(&home, usage_url, oauth_url).probe().await;
            assert_eq!(outcome.result.unwrap_err(), expected);
            if status == 429 {
                assert!(outcome.retry_after.unwrap() > Utc::now());
            } else {
                assert!(outcome.retry_after.is_none());
            }
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_oversized_usage_body_but_keeps_retry_after() {
        let home = TestHome::new();
        home.write_current_auth("access", None);
        let (listener, usage_url) = server();
        let oauth_url = Url::parse(&format!("http://{}/oauth", usage_url.authority())).unwrap();
        let task = tokio::spawn(async move {
            let request = read_request(listener.accept().await.unwrap().0).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nRetry-After: 120\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_RESPONSE_BYTES + 1
            );
            let mut stream = request.stream;
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let outcome = probe(&home, usage_url, oauth_url).probe().await;
        assert_eq!(outcome.result.unwrap_err(), ErrorCode::ProviderError);
        assert!(outcome.retry_after.unwrap() > Utc::now());
        task.await.unwrap();
    }

    #[test]
    fn recognizes_expired_reused_and_invalidated_refresh_tokens_without_exposing_text() {
        for code in [
            "refresh_token_expired",
            "refresh_token_reused",
            "refresh_token_invalidated",
        ] {
            let body = serde_json::to_vec(&serde_json::json!({"error":{"code":code}})).unwrap();
            assert!(super::is_rejected_refresh_token(&body));
        }
        assert!(!super::is_rejected_refresh_token(b"private upstream error"));
    }

    #[test]
    fn authorization_and_account_headers_are_sensitive_and_tokens_are_validated() {
        let secret = SecretString::new("synthetic-secret".to_owned());
        let header = super::authorization_header(&secret).unwrap();
        assert!(header.is_sensitive());
        assert!(
            super::account_header(&SecretString::new("synthetic-account".to_owned()))
                .unwrap()
                .is_sensitive()
        );
        assert!(super::authorization_header(&SecretString::new("bad\r\nheader".into())).is_none());
        assert!(super::account_header(&SecretString::new("bad\r\naccount".into())).is_err());
    }
}
