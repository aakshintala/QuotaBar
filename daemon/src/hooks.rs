use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Bytes;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::feed::FeedDto;
use crate::feed_text::{BucketKey, EmittedBucketSeverity, format_feed};
use crate::quota::Severity;
use crate::scheduler::{RequestError, SchedulerClient};

const SESSION_START_REFRESH_WAIT: Duration = Duration::from_millis(1_500);
const MAX_SESSIONS: usize = 1_024;
const SESSION_IDLE_LIMIT: ChronoDuration = ChronoDuration::hours(24);

pub(crate) struct HookService {
    scheduler: SchedulerClient,
    history: Mutex<SessionHistory>,
}

impl HookService {
    pub(crate) fn new(scheduler: SchedulerClient) -> Self {
        Self {
            scheduler,
            history: Mutex::new(SessionHistory::default()),
        }
    }

    pub(crate) async fn session_start(&self, body: Bytes) -> Result<HookResponse, HookError> {
        let session_id = parse_session_id(&body)?;
        drop(body);

        let feed = self
            .scheduler
            .refresh(SESSION_START_REFRESH_WAIT)
            .await
            .map_err(HookError::Scheduler)?;
        let formatted = format_feed(&feed, Utc::now(), None);
        if let Some(session_id) = session_id {
            self.lock_history().record_session_start(
                session_id,
                &formatted.emitted_buckets,
                Utc::now(),
            );
        }

        Ok(if formatted.text.is_empty() {
            HookResponse::empty()
        } else {
            HookResponse::context("SessionStart", formatted.text)
        })
    }

    pub(crate) async fn prompt(&self, body: Bytes) -> Result<HookResponse, HookError> {
        let session_id = parse_session_id(&body)?;
        drop(body);

        // cached() reloads and validates daemon configuration at this request
        // boundary, but never starts or joins provider work.
        let feed = self
            .scheduler
            .cached()
            .await
            .map_err(HookError::Scheduler)?;
        let Some(session_id) = session_id else {
            return Ok(HookResponse::empty());
        };

        let current = current_bucket_severities(&feed);
        let worsened =
            self.lock_history()
                .compare_and_update_prompt(session_id, current, Utc::now());
        if worsened.is_empty() {
            return Ok(HookResponse::empty());
        }

        let formatted = format_feed(&feed, Utc::now(), Some(&worsened));
        Ok(if formatted.text.is_empty() {
            HookResponse::empty()
        } else {
            HookResponse::context("UserPromptSubmit", formatted.text)
        })
    }

    fn lock_history(&self) -> std::sync::MutexGuard<'_, SessionHistory> {
        self.history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug)]
pub(crate) enum HookError {
    InvalidJson,
    Scheduler(RequestError),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HookResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HookSpecificOutput {
    hook_event_name: &'static str,
    additional_context: String,
}

impl HookResponse {
    fn empty() -> Self {
        Self {
            hook_specific_output: None,
        }
    }

    fn context(event_name: &'static str, additional_context: String) -> Self {
        Self {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: event_name,
                additional_context,
            }),
        }
    }
}

struct HookInput {
    session_id: Option<String>,
}

impl<'de> Deserialize<'de> for HookInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct InputVisitor;

        impl<'de> Visitor<'de> for InputVisitor {
            type Value = HookInput;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a hook input object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut session_id = None;
                let mut saw_session_id = false;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "session_id" {
                        if saw_session_id {
                            return Err(serde::de::Error::duplicate_field("session_id"));
                        }
                        saw_session_id = true;
                        session_id = map.next_value::<MaybeSessionId>()?.0;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(HookInput { session_id })
            }
        }

        deserializer.deserialize_map(InputVisitor)
    }
}

struct MaybeSessionId(Option<String>);

impl<'de> Deserialize<'de> for MaybeSessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SessionIdVisitor;

        impl<'de> Visitor<'de> for SessionIdVisitor {
            type Value = MaybeSessionId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("any JSON value")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MaybeSessionId(Some(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MaybeSessionId(Some(value)))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MaybeSessionId(None))
            }

            fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MaybeSessionId(None))
            }

            fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MaybeSessionId(None))
            }

            fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MaybeSessionId(None))
            }

            fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MaybeSessionId(None))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while sequence.next_element::<IgnoredAny>()?.is_some() {}
                Ok(MaybeSessionId(None))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(MaybeSessionId(None))
            }
        }

        deserializer.deserialize_any(SessionIdVisitor)
    }
}

fn parse_session_id(body: &[u8]) -> Result<Option<String>, HookError> {
    let input: HookInput = serde_json::from_slice(body).map_err(|_| HookError::InvalidJson)?;
    Ok(input.session_id.filter(|value| valid_session_id(value)))
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.chars().any(char::is_control)
        && !value.chars().all(char::is_whitespace)
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct BucketIdentity {
    provider_id: String,
    bucket_key: String,
}

#[derive(Default)]
struct SessionHistory {
    sessions: HashMap<String, SessionRecord>,
    least_recently_used: VecDeque<String>,
}

struct SessionRecord {
    last_activity: DateTime<Utc>,
    buckets: HashMap<BucketIdentity, Severity>,
}

impl SessionHistory {
    fn record_session_start(
        &mut self,
        session_id: String,
        emitted_buckets: &[EmittedBucketSeverity],
        now: DateTime<Utc>,
    ) {
        let buckets = emitted_buckets
            .iter()
            .map(|bucket| {
                (
                    BucketIdentity {
                        provider_id: bucket.provider_id.clone(),
                        bucket_key: bucket.bucket_key.clone(),
                    },
                    bucket.severity,
                )
            })
            .collect();
        self.replace(session_id, buckets, now);
    }

    fn compare_and_update_prompt(
        &mut self,
        session_id: String,
        current: HashMap<BucketIdentity, Severity>,
        now: DateTime<Utc>,
    ) -> Vec<BucketKey> {
        self.remove_expired(now);
        let previous = self.sessions.get(&session_id);
        let worsened = current
            .iter()
            .filter_map(|(identity, severity)| {
                let previous_severity = previous
                    .and_then(|record| record.buckets.get(identity))
                    .copied()
                    .unwrap_or(Severity::Healthy);
                (*severity > previous_severity).then(|| BucketKey {
                    provider_id: identity.provider_id.clone(),
                    key: identity.bucket_key.clone(),
                })
            })
            .collect();
        self.replace(session_id, current, now);
        worsened
    }

    fn replace(
        &mut self,
        session_id: String,
        buckets: HashMap<BucketIdentity, Severity>,
        now: DateTime<Utc>,
    ) {
        self.remove_expired(now);
        self.least_recently_used
            .retain(|existing| existing != &session_id);
        self.sessions.insert(
            session_id.clone(),
            SessionRecord {
                last_activity: now,
                buckets,
            },
        );
        self.least_recently_used.push_back(session_id);
        while self.sessions.len() > MAX_SESSIONS {
            if let Some(oldest) = self.least_recently_used.pop_front() {
                self.sessions.remove(&oldest);
            }
        }
    }

    fn remove_expired(&mut self, now: DateTime<Utc>) {
        self.sessions.retain(|_, record| {
            record.last_activity <= now
                && now.signed_duration_since(record.last_activity) < SESSION_IDLE_LIMIT
        });
        self.least_recently_used
            .retain(|session_id| self.sessions.contains_key(session_id));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.sessions.len()
    }
}

fn current_bucket_severities(feed: &FeedDto) -> HashMap<BucketIdentity, Severity> {
    let mut buckets = HashMap::new();
    for provider in &feed.providers {
        for quota in &provider.quotas {
            let severity = match quota.status.as_str() {
                "healthy" => Severity::Healthy,
                "warning" => Severity::Warning,
                "critical" => Severity::Critical,
                "depleted" => Severity::Depleted,
                _ => continue,
            };
            buckets.insert(
                BucketIdentity {
                    provider_id: provider.id.clone(),
                    bucket_key: quota.key.clone(),
                },
                severity,
            );
        }
    }
    buckets
}

#[cfg(test)]
mod tests {
    use super::{
        BucketIdentity, HookResponse, SessionHistory, current_bucket_severities, parse_session_id,
    };
    use crate::feed::{FeedDto, ProviderDto, QuotaDto};
    use crate::feed_text::{EmittedBucketSeverity, MAX_FEED_TEXT_CHARS, format_feed};
    use crate::quota::Severity;
    use chrono::{DateTime, Duration, Utc};
    use serde_json::Value;
    use std::collections::HashMap;

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    fn bucket(provider: &str, key: &str, severity: Severity) -> EmittedBucketSeverity {
        EmittedBucketSeverity {
            provider_id: provider.to_owned(),
            bucket_key: key.to_owned(),
            severity,
        }
    }

    fn current(entries: &[(&str, &str, Severity)]) -> HashMap<BucketIdentity, Severity> {
        entries
            .iter()
            .map(|(provider, key, severity)| {
                (
                    BucketIdentity {
                        provider_id: (*provider).to_owned(),
                        bucket_key: (*key).to_owned(),
                    },
                    *severity,
                )
            })
            .collect()
    }

    fn feed(statuses: &[(&str, &str)]) -> FeedDto {
        FeedDto {
            schema_version: 1,
            generated_at: "2027-01-15T08:00:00Z".to_owned(),
            disabled_provider_ids: Vec::new(),
            providers: vec![ProviderDto {
                id: "claude".to_owned(),
                name: "Claude".to_owned(),
                tier: None,
                captured_at: None,
                age_seconds: None,
                freshness: "unknown".to_owned(),
                error_code: None,
                status: "unknown".to_owned(),
                quotas: statuses
                    .iter()
                    .map(|(key, status)| QuotaDto {
                        key: (*key).to_owned(),
                        label: (*key).to_owned(),
                        percent_remaining: None,
                        balance_remaining: None,
                        balance_used: None,
                        balance_cap: None,
                        balance_unit: None,
                        units_used: None,
                        units_limit: None,
                        resets_at: None,
                        reset_text: None,
                        status: (*status).to_owned(),
                    })
                    .collect(),
            }],
        }
    }

    #[test]
    fn parses_only_valid_opaque_ids_and_enforces_utf8_byte_bounds() {
        assert_eq!(
            parse_session_id(br#"{"session_id":"abc123"}"#).unwrap(),
            Some("abc123".to_owned())
        );
        let unicode_256 = "é".repeat(128);
        let unicode_257 = format!("{}a", unicode_256);
        let valid = serde_json::to_vec(&serde_json::json!({"session_id": unicode_256})).unwrap();
        let invalid = serde_json::to_vec(&serde_json::json!({"session_id": unicode_257})).unwrap();
        assert!(parse_session_id(&valid).unwrap().is_some());
        assert_eq!(parse_session_id(&invalid).unwrap(), None);
        for value in ["", " \t\n", "bad\u{0000}id"] {
            let body = serde_json::to_vec(&serde_json::json!({"session_id": value})).unwrap();
            assert_eq!(parse_session_id(&body).unwrap(), None);
        }
        for value in ["  opaque but not normalized  ", "not-a-uuid", "雪"] {
            let body = serde_json::to_vec(&serde_json::json!({"session_id": value})).unwrap();
            assert_eq!(parse_session_id(&body).unwrap().as_deref(), Some(value));
        }
        for body in [
            br#"{}"#.as_slice(),
            br#"{"session_id":null}"#.as_slice(),
            br#"{"session_id":42}"#.as_slice(),
            br#"{"session_id":true}"#.as_slice(),
            br#"{"session_id":[]}"#.as_slice(),
            br#"{"session_id":{}}"#.as_slice(),
        ] {
            assert_eq!(parse_session_id(body).unwrap(), None);
        }
    }

    #[test]
    fn malformed_json_and_duplicate_session_ids_fail_without_echoing_input() {
        let malformed = br#"{"session_id":"id","prompt":"private text""#;
        assert!(parse_session_id(malformed).is_err());
        assert!(parse_session_id(br#"{"session_id":"a","session_id":"b"}"#).is_err());
    }

    #[test]
    fn prompt_history_defaults_to_healthy_rearms_and_replaces_removed_buckets() {
        let mut history = SessionHistory::default();
        let now = time(1_000);
        assert_eq!(
            history.compare_and_update_prompt(
                "session".to_owned(),
                current(&[("claude", "week", Severity::Warning)]),
                now,
            ),
            [crate::feed_text::BucketKey {
                provider_id: "claude".to_owned(),
                key: "week".to_owned(),
            }]
        );
        assert!(
            history
                .compare_and_update_prompt(
                    "session".to_owned(),
                    current(&[("claude", "week", Severity::Warning)]),
                    now + Duration::seconds(1),
                )
                .is_empty()
        );
        assert!(
            history
                .compare_and_update_prompt(
                    "session".to_owned(),
                    current(&[("claude", "week", Severity::Healthy)]),
                    now + Duration::seconds(2),
                )
                .is_empty()
        );
        assert_eq!(
            history
                .compare_and_update_prompt(
                    "session".to_owned(),
                    current(&[("claude", "week", Severity::Warning)]),
                    now + Duration::seconds(3),
                )
                .len(),
            1
        );
        history.compare_and_update_prompt(
            "session".to_owned(),
            current(&[]),
            now + Duration::seconds(4),
        );
        assert_eq!(
            history
                .compare_and_update_prompt(
                    "session".to_owned(),
                    current(&[("claude", "week", Severity::Critical)]),
                    now + Duration::seconds(5),
                )
                .len(),
            1,
            "a removed bucket reappears against healthy"
        );
    }

    #[test]
    fn valid_opaque_sessions_keep_independent_history() {
        let mut history = SessionHistory::default();
        let now = time(1_000);
        history.record_session_start(
            "not-a-uuid".to_owned(),
            &[bucket("claude", "week", Severity::Warning)],
            now,
        );
        assert!(
            history
                .compare_and_update_prompt(
                    "second-session".to_owned(),
                    current(&[("claude", "week", Severity::Warning)]),
                    now + Duration::seconds(1),
                )
                .len()
                == 1
        );
        assert!(
            history
                .compare_and_update_prompt(
                    "not-a-uuid".to_owned(),
                    current(&[("claude", "week", Severity::Warning)]),
                    now + Duration::seconds(2),
                )
                .is_empty()
        );
    }

    #[test]
    fn session_start_replaces_history_with_only_buckets_actually_emitted() {
        let mut history = SessionHistory::default();
        let now = time(1_000);
        history.record_session_start(
            "session".to_owned(),
            &[bucket("claude", "same", Severity::Warning)],
            now,
        );
        assert!(
            history
                .compare_and_update_prompt(
                    "session".to_owned(),
                    current(&[
                        ("claude", "same", Severity::Warning),
                        ("claude", "truncated", Severity::Warning),
                    ]),
                    now + Duration::seconds(1),
                )
                .iter()
                .any(|key| key.key == "truncated")
        );
        history.record_session_start(
            "session".to_owned(),
            &[bucket("claude", "same", Severity::Critical)],
            now + Duration::seconds(2),
        );
        assert!(
            history
                .compare_and_update_prompt(
                    "session".to_owned(),
                    current(&[("claude", "same", Severity::Critical)]),
                    now + Duration::seconds(3),
                )
                .is_empty()
        );
    }

    #[test]
    fn startup_and_prompt_truncation_track_only_emitted_start_keys_and_all_prompt_keys() {
        let now = time(1_000);
        let mut feed = feed(&[]);
        feed.providers[0].quotas = (0..160)
            .map(|index| QuotaDto {
                key: format!("bucket-{index}"),
                label: "long quota label ".repeat(8),
                percent_remaining: Some(40.0),
                balance_remaining: None,
                balance_used: None,
                balance_cap: None,
                balance_unit: None,
                units_used: None,
                units_limit: None,
                resets_at: None,
                reset_text: None,
                status: "warning".to_owned(),
            })
            .collect();

        let startup = format_feed(&feed, now, None);
        assert!(startup.text.chars().count() <= MAX_FEED_TEXT_CHARS);
        assert!(startup.emitted_buckets.len() < feed.providers[0].quotas.len());
        let mut history = SessionHistory::default();
        history.record_session_start("session".to_owned(), &startup.emitted_buckets, now);

        let all_current = current_bucket_severities(&feed);
        let omitted_warnings = history.compare_and_update_prompt(
            "session".to_owned(),
            all_current.clone(),
            now + Duration::seconds(1),
        );
        assert_eq!(
            omitted_warnings.len(),
            feed.providers[0].quotas.len() - startup.emitted_buckets.len()
        );
        let prompt = format_feed(&feed, now, Some(&omitted_warnings));
        assert!(prompt.text.chars().count() <= MAX_FEED_TEXT_CHARS);
        assert_eq!(
            history.sessions["session"].buckets.len(),
            feed.providers[0].quotas.len(),
            "prompt history replaces from every current recognized bucket"
        );
        assert!(
            history
                .compare_and_update_prompt(
                    "session".to_owned(),
                    all_current,
                    now + Duration::seconds(2),
                )
                .is_empty()
        );
    }

    #[test]
    fn history_expires_at_exactly_24_hours_and_expires_future_activity_on_rollback() {
        let mut history = SessionHistory::default();
        let now = time(1_000);
        history.record_session_start("old".to_owned(), &[], now);
        history.record_session_start("future".to_owned(), &[], now + Duration::seconds(1));
        history.remove_expired(now + Duration::hours(24) - Duration::seconds(1));
        assert_eq!(history.len(), 2);
        history.remove_expired(now + Duration::hours(24));
        assert_eq!(history.len(), 1);
        history.remove_expired(now);
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn history_is_lru_bounded_to_1024_sessions() {
        let mut history = SessionHistory::default();
        let now = time(1_000);
        for index in 0..1_024 {
            history.record_session_start(format!("session-{index}"), &[], now);
        }
        assert_eq!(history.len(), 1_024);
        // Touch the oldest entry so the next insertion evicts the second-oldest.
        history.compare_and_update_prompt(
            "session-0".to_owned(),
            HashMap::new(),
            now + Duration::seconds(1),
        );
        history.record_session_start("session-1024".to_owned(), &[], now + Duration::seconds(2));
        assert_eq!(history.len(), 1_024);
        assert!(history.sessions.contains_key("session-0"));
        assert!(!history.sessions.contains_key("session-1"));
        assert!(history.sessions.contains_key("session-1024"));
    }

    #[test]
    fn parsing_ignores_prompt_and_current_severity_skips_unknown_provider_statuses() {
        let prompt = "do not retain this text";
        let body = serde_json::to_vec(
            &serde_json::json!({"session_id":"opaque","prompt":prompt,"other":{"nested":[1,2,3]}}),
        )
        .unwrap();
        assert_eq!(parse_session_id(&body).unwrap().as_deref(), Some("opaque"));
        let feed = feed(&[("good", "warning"), ("unknown", "provider_specific")]);
        let current = current_bucket_severities(&feed);
        assert_eq!(current.len(), 1);
        assert!(current.contains_key(&BucketIdentity {
            provider_id: "claude".to_owned(),
            bucket_key: "good".to_owned(),
        }));
        assert!(!current.keys().any(|key| key.bucket_key == prompt));
    }

    #[test]
    fn response_serializes_official_hook_event_envelope_and_empty_object() {
        let empty = serde_json::to_value(HookResponse::empty()).unwrap();
        assert_eq!(empty, Value::Object(serde_json::Map::new()));
        let context = serde_json::to_value(HookResponse::context(
            "UserPromptSubmit",
            "Quota context".to_owned(),
        ))
        .unwrap();
        assert_eq!(
            context["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert_eq!(
            context["hookSpecificOutput"]["additionalContext"],
            "Quota context"
        );
    }
}
