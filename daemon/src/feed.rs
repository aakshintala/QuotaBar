use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::quota::{Observation, Quota, Severity};

const FRESH_FOR_SECONDS: i64 = 300;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    CredentialsMissing,
    LoginExpired,
    KeychainBlocked,
    SubscriptionUnavailable,
    Cooldown,
    RateLimited,
    NetworkError,
    ConfigurationError,
    Busy,
    ProviderError,
    PersistenceError,
    InternalError,
}

/// Projection input supplied by the daemon's provider/cache layer.
#[derive(Clone, Copy)]
pub struct ProviderState<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub observation: Option<&'a Observation>,
    pub error_code: Option<ErrorCode>,
    pub busy: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedDto {
    pub schema_version: u8,
    pub generated_at: String,
    pub disabled_provider_ids: Vec<String>,
    pub providers: Vec<ProviderDto>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDto {
    pub id: String,
    pub name: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub tier: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub captured_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub age_seconds: Option<u64>,
    pub freshness: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub error_code: Option<ErrorCode>,
    pub status: String,
    pub quotas: Vec<QuotaDto>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaDto {
    pub key: String,
    pub label: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub percent_remaining: Option<f64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub balance_remaining: Option<f64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub balance_used: Option<f64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub balance_cap: Option<f64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub balance_unit: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub units_used: Option<i64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub units_limit: Option<i64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub resets_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reset_text: Option<String>,
    pub status: String,
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedDecodeError {
    InvalidPayload,
    UnsupportedSchema,
}

impl std::fmt::Display for FeedDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPayload => formatter.write_str("quota feed is invalid"),
            Self::UnsupportedSchema => formatter.write_str("quota feed schema is unsupported"),
        }
    }
}

impl std::error::Error for FeedDecodeError {}

/// Decode the existing v1 DTO shape without exposing parser details.
pub fn decode_feed_v1(bytes: &[u8]) -> Result<FeedDto, FeedDecodeError> {
    let feed: FeedDto =
        serde_json::from_slice(bytes).map_err(|_| FeedDecodeError::InvalidPayload)?;
    if feed.schema_version != 1 {
        return Err(FeedDecodeError::UnsupportedSchema);
    }
    Ok(feed)
}

/// Build the v1 wire representation at an explicit response time. Disabled
/// IDs are retained even when no corresponding provider is currently known.
pub fn project_feed(
    now: &DateTime<Utc>,
    providers: &[ProviderState<'_>],
    disabled_provider_ids: &[String],
) -> FeedDto {
    let providers = providers
        .iter()
        .filter(|provider| !disabled_provider_ids.iter().any(|id| id == provider.id))
        .map(|provider| project_provider(now, provider))
        .collect();

    FeedDto {
        schema_version: 1,
        generated_at: format_time(now),
        disabled_provider_ids: disabled_provider_ids.to_vec(),
        providers,
    }
}

fn project_provider(now: &DateTime<Utc>, state: &ProviderState<'_>) -> ProviderDto {
    let error_code = state
        .error_code
        .or_else(|| state.busy.then_some(ErrorCode::Busy));
    let Some(observation) = state.observation.filter(|value| &value.captured_at <= now) else {
        return ProviderDto {
            id: state.id.to_owned(),
            name: state.name.to_owned(),
            tier: None,
            captured_at: None,
            age_seconds: None,
            freshness: "unknown".to_owned(),
            error_code,
            status: "unknown".to_owned(),
            quotas: Vec::new(),
        };
    };

    let age = now.signed_duration_since(observation.captured_at);
    let current_quotas: Vec<_> = observation
        .quotas
        .iter()
        .filter(|quota| quota.resets_at.as_ref().is_none_or(|reset| reset > now))
        .collect();
    let status = current_quotas
        .iter()
        .map(|quota| quota.status(now))
        .max()
        .map_or("unknown", Severity::as_str);
    let quotas = current_quotas
        .into_iter()
        .map(|quota| project_quota(now, quota))
        .collect();

    ProviderDto {
        id: state.id.to_owned(),
        name: state.name.to_owned(),
        tier: observation.tier.clone(),
        captured_at: Some(format_time(&observation.captured_at)),
        age_seconds: Some(age.num_seconds().max(0) as u64),
        freshness: if age < Duration::seconds(FRESH_FOR_SECONDS) {
            "fresh".to_owned()
        } else {
            "stale".to_owned()
        },
        error_code,
        status: status.to_owned(),
        quotas,
    }
}

fn project_quota(now: &DateTime<Utc>, quota: &Quota) -> QuotaDto {
    QuotaDto {
        key: quota.key.clone(),
        label: quota.label.clone(),
        percent_remaining: quota.percent_remaining,
        balance_remaining: quota.balance_remaining,
        balance_used: quota.balance_used,
        balance_cap: quota.balance_cap,
        balance_unit: quota.balance_unit.clone(),
        units_used: quota.units_used,
        units_limit: quota.units_limit,
        resets_at: quota.resets_at.as_ref().map(format_time),
        reset_text: quota
            .resets_at
            .as_ref()
            .map(|reset| reset_text(reset.signed_duration_since(*now))),
        status: quota.status(now).as_str().to_owned(),
    }
}

fn reset_text(until: Duration) -> String {
    let seconds = until.num_seconds();
    let compact = if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        return "Resets soon".to_owned();
    };
    format!("Resets in {compact}")
}

fn format_time(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

#[cfg(test)]
mod tests {
    use super::{ErrorCode, FeedDecodeError, ProviderState, decode_feed_v1, project_feed};
    use crate::quota::{Observation, Quota};
    use chrono::{DateTime, Duration, Utc};
    use serde_json::Value;

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(seconds, 0).unwrap()
    }

    fn percent_quota(
        percent: f64,
        resets_at: Option<DateTime<Utc>>,
        duration: Option<i64>,
    ) -> Quota {
        Quota {
            key: "session".into(),
            label: "Session".into(),
            percent_remaining: Some(percent),
            balance_remaining: None,
            balance_used: None,
            balance_cap: None,
            balance_unit: None,
            units_used: None,
            units_limit: None,
            resets_at,
            window_duration_seconds: duration,
        }
    }

    fn observation(captured_at: DateTime<Utc>, quotas: Vec<Quota>) -> Observation {
        Observation {
            captured_at,
            tier: Some("Pro".into()),
            quotas,
        }
    }

    fn provider<'a>(id: &'a str, observation: Option<&'a Observation>) -> ProviderState<'a> {
        ProviderState {
            id,
            name: "Provider",
            observation,
            error_code: None,
            busy: false,
        }
    }

    #[test]
    fn projects_schema_v1_and_exact_freshness_boundary() {
        let captured = time(1_800_000_000);
        let value = observation(captured, vec![percent_quota(75.0, None, None)]);
        let state = provider("unknown-provider", Some(&value));
        let fresh = project_feed(&(captured + Duration::seconds(299)), &[state], &[]);
        assert_eq!(fresh.schema_version, 1);
        assert_eq!(fresh.providers[0].freshness, "fresh");
        assert_eq!(fresh.providers[0].age_seconds, Some(299));
        let stale = project_feed(&(captured + Duration::seconds(300)), &[state], &[]);
        assert_eq!(stale.providers[0].freshness, "stale");
        assert_eq!(stale.providers[0].age_seconds, Some(300));
    }

    #[test]
    fn computes_bucket_status_at_response_time_without_a_new_observation() {
        let captured = time(1_800_000_000);
        let reset = captured + Duration::hours(4);
        let value = observation(
            captured,
            vec![percent_quota(10.0, Some(reset), Some(5 * 3600))],
        );
        let state = provider("claude", Some(&value));

        let before = project_feed(&captured, &[state], &[]);
        let later = project_feed(
            &(captured + Duration::hours(3) + Duration::minutes(30)),
            &[state],
            &[],
        );
        assert_eq!(before.providers[0].quotas[0].status, "critical");
        assert_eq!(later.providers[0].quotas[0].status, "healthy");
        assert_eq!(
            before.providers[0].captured_at,
            later.providers[0].captured_at
        );
    }

    #[test]
    fn response_time_reset_text_and_provider_status_use_current_buckets() {
        let now = time(1_800_000_000);
        let reset = now + Duration::hours(2);
        let value = observation(
            now,
            vec![
                percent_quota(10.0, Some(reset), None),
                percent_quota(0.0, Some(reset), None),
            ],
        );
        let feed = project_feed(&now, &[provider("claude", Some(&value))], &[]);

        assert_eq!(feed.providers[0].status, "depleted");
        assert_eq!(
            feed.providers[0].quotas[0].reset_text.as_deref(),
            Some("Resets in 2h")
        );
        assert_eq!(
            feed.providers[0].quotas[0].resets_at.as_deref(),
            Some("2027-01-15T10:00:00Z")
        );
    }

    #[test]
    fn removes_expired_windows_and_hides_future_observations() {
        let now = time(1_800_000_000);
        let expired = observation(
            now - Duration::seconds(10),
            vec![percent_quota(50.0, Some(now), None)],
        );
        let future = observation(
            now + Duration::seconds(1),
            vec![percent_quota(50.0, None, None)],
        );
        let states = [
            provider("expired", Some(&expired)),
            provider("future", Some(&future)),
        ];
        let feed = project_feed(&now, &states, &[]);

        assert_eq!(feed.providers[0].status, "unknown");
        assert!(feed.providers[0].quotas.is_empty());
        assert!(feed.providers[0].captured_at.is_some());
        assert_eq!(feed.providers[1].status, "unknown");
        assert_eq!(feed.providers[1].freshness, "unknown");
        assert_eq!(feed.providers[1].captured_at, None);
        assert_eq!(feed.providers[1].age_seconds, None);
        assert!(feed.providers[1].quotas.is_empty());
    }

    #[test]
    fn serializes_balances_as_numbers_and_nullable_fields_as_null() {
        let now = time(1_800_000_000);
        let mut quota = percent_quota(0.0, None, None);
        quota.percent_remaining = None;
        quota.balance_remaining = Some(1.25);
        quota.balance_used = Some(3.5);
        quota.balance_cap = Some(4.75);
        quota.balance_unit = Some("usd".into());
        let value = observation(now, vec![quota]);
        let feed = project_feed(&now, &[provider("codex", Some(&value))], &[]);
        let json = serde_json::to_value(feed).unwrap();
        let bucket = &json["providers"][0]["quotas"][0];

        assert!(bucket["balanceRemaining"].is_number());
        assert_eq!(bucket["balanceRemaining"], 1.25);
        assert_eq!(bucket["balanceUsed"], 3.5);
        assert_eq!(bucket["balanceCap"], 4.75);
        assert_eq!(bucket["balanceUnit"], "usd");
        for key in [
            "percentRemaining",
            "unitsUsed",
            "unitsLimit",
            "resetsAt",
            "resetText",
        ] {
            assert_eq!(bucket[key], Value::Null, "{key} must be present as null");
        }
        assert_eq!(bucket["status"], "healthy");
        assert_eq!(bucket.as_object().unwrap().len(), 12);
    }

    #[test]
    fn projects_count_fields_and_uses_them_for_status() {
        let now = time(1_800_000_000);
        let mut quota = percent_quota(100.0, None, None);
        quota.percent_remaining = None;
        quota.units_used = Some(90);
        quota.units_limit = Some(100);
        let value = observation(now, vec![quota]);
        let json =
            serde_json::to_value(project_feed(&now, &[provider("cursor", Some(&value))], &[]))
                .unwrap();
        let bucket = &json["providers"][0]["quotas"][0];

        assert_eq!(bucket["unitsUsed"], 90);
        assert_eq!(bucket["unitsLimit"], 100);
        assert_eq!(bucket["percentRemaining"], Value::Null);
        assert_eq!(bucket["status"], "critical");
    }

    #[test]
    fn disabled_ids_are_listed_and_cached_data_is_hidden() {
        let now = time(1_800_000_000);
        let value = observation(now, vec![percent_quota(50.0, None, None)]);
        let states = [
            provider("muse", Some(&value)),
            provider("future-provider", None),
        ];
        let disabled = vec!["muse".to_owned(), "unknown-disabled".to_owned()];
        let feed = project_feed(&now, &states, &disabled);

        assert_eq!(feed.disabled_provider_ids, disabled);
        assert_eq!(feed.providers.len(), 1);
        assert_eq!(feed.providers[0].id, "future-provider");
        assert_eq!(feed.providers[0].status, "unknown");
    }

    #[test]
    fn preserves_stale_reading_on_error_and_uses_busy_only_as_fallback() {
        let captured = time(1_800_000_000);
        let value = observation(captured, vec![percent_quota(30.0, None, None)]);
        let mut failed = provider("codex", Some(&value));
        failed.error_code = Some(ErrorCode::NetworkError);
        failed.busy = true;
        let mut busy = provider("muse", None);
        busy.busy = true;
        let feed = project_feed(&(captured + Duration::seconds(300)), &[failed, busy], &[]);

        assert_eq!(feed.providers[0].freshness, "stale");
        assert_eq!(feed.providers[0].error_code, Some(ErrorCode::NetworkError));
        assert_eq!(feed.providers[0].quotas.len(), 1);
        assert_eq!(feed.providers[1].error_code, Some(ErrorCode::Busy));
        assert_eq!(feed.providers[1].status, "unknown");
        assert!(feed.providers[1].quotas.is_empty());
    }

    #[test]
    fn unknown_enabled_provider_is_representable_without_observation() {
        let state = provider("not-yet-implemented", None);
        let feed = project_feed(&time(1_800_000_000), &[state], &[]);
        assert_eq!(feed.providers[0].id, "not-yet-implemented");
        assert_eq!(feed.providers[0].name, "Provider");
        assert_eq!(feed.providers[0].status, "unknown");
        assert_eq!(feed.providers[0].freshness, "unknown");
        assert_eq!(feed.providers[0].quotas.len(), 0);
    }

    #[test]
    fn emits_all_nullable_provider_fields_and_rfc3339_utc_times() {
        let now = time(1_800_000_000);
        let feed = project_feed(&now, &[provider("provider", None)], &[]);
        let json = serde_json::to_value(feed).unwrap();
        let provider = &json["providers"][0];
        assert_eq!(json["schemaVersion"], 1);
        assert_eq!(json["generatedAt"], "2027-01-15T08:00:00Z");
        assert_eq!(provider["capturedAt"], Value::Null);
        assert_eq!(provider["ageSeconds"], Value::Null);
        assert_eq!(provider["tier"], Value::Null);
        assert_eq!(provider["errorCode"], Value::Null);
        assert_eq!(provider["freshness"], "unknown");
        assert_eq!(json.as_object().unwrap().len(), 4);
        assert_eq!(provider.as_object().unwrap().len(), 9);

        let codes = [
            ErrorCode::CredentialsMissing,
            ErrorCode::LoginExpired,
            ErrorCode::KeychainBlocked,
            ErrorCode::SubscriptionUnavailable,
            ErrorCode::Cooldown,
            ErrorCode::RateLimited,
            ErrorCode::NetworkError,
            ErrorCode::ConfigurationError,
            ErrorCode::Busy,
            ErrorCode::ProviderError,
            ErrorCode::PersistenceError,
            ErrorCode::InternalError,
        ]
        .map(|code| serde_json::to_value(code).unwrap());
        assert_eq!(
            codes,
            [
                "credentials_missing",
                "login_expired",
                "keychain_blocked",
                "subscription_unavailable",
                "cooldown",
                "rate_limited",
                "network_error",
                "configuration_error",
                "busy",
                "provider_error",
                "persistence_error",
                "internal_error",
            ]
        );
    }

    fn deserialization_fixture() -> Value {
        let now = time(1_800_000_000);
        let snapshot = observation(now, vec![percent_quota(75.0, None, None)]);
        serde_json::to_value(project_feed(
            &now,
            &[provider("claude", Some(&snapshot))],
            &[],
        ))
        .unwrap()
    }

    #[test]
    fn every_nullable_v1_property_accepts_null_but_rejects_omission() {
        let fields = [
            ("provider", "tier"),
            ("provider", "capturedAt"),
            ("provider", "ageSeconds"),
            ("provider", "errorCode"),
            ("quota", "percentRemaining"),
            ("quota", "balanceRemaining"),
            ("quota", "balanceUsed"),
            ("quota", "balanceCap"),
            ("quota", "balanceUnit"),
            ("quota", "unitsUsed"),
            ("quota", "unitsLimit"),
            ("quota", "resetsAt"),
            ("quota", "resetText"),
        ];

        for (owner, field) in fields {
            let mut explicit_null = deserialization_fixture();
            let object = if owner == "provider" {
                explicit_null["providers"][0].as_object_mut().unwrap()
            } else {
                explicit_null["providers"][0]["quotas"][0]
                    .as_object_mut()
                    .unwrap()
            };
            object.insert(field.to_owned(), Value::Null);
            let bytes = serde_json::to_vec(&explicit_null).unwrap();
            assert!(decode_feed_v1(&bytes).is_ok(), "{owner}.{field} null");

            let mut omitted = deserialization_fixture();
            let object = if owner == "provider" {
                omitted["providers"][0].as_object_mut().unwrap()
            } else {
                omitted["providers"][0]["quotas"][0]
                    .as_object_mut()
                    .unwrap()
            };
            object.remove(field);
            let bytes = serde_json::to_vec(&omitted).unwrap();
            assert!(
                matches!(decode_feed_v1(&bytes), Err(FeedDecodeError::InvalidPayload)),
                "{owner}.{field} omission"
            );
        }
    }

    #[test]
    fn decoder_requires_schema_one_and_returns_only_static_errors() {
        assert_eq!(
            decode_feed_v1(br#"{"schemaVersion":2}"#).unwrap_err(),
            FeedDecodeError::InvalidPayload
        );
        let mut wrong_schema = deserialization_fixture();
        wrong_schema["schemaVersion"] = 2.into();
        assert_eq!(
            decode_feed_v1(&serde_json::to_vec(&wrong_schema).unwrap()).unwrap_err(),
            FeedDecodeError::UnsupportedSchema
        );
        let mut wrong_type = deserialization_fixture();
        wrong_type["providers"][0]["id"] = serde_json::json!({
            "secret": "synthetic-sensitive-input"
        });
        let error = decode_feed_v1(&serde_json::to_vec(&wrong_type).unwrap()).unwrap_err();
        assert_eq!(error.to_string(), "quota feed is invalid");
        assert!(!error.to_string().contains("synthetic-sensitive-input"));
    }

    #[test]
    fn decoder_keeps_unknown_fields_allowed_at_each_object_level() {
        let mut value = deserialization_fixture();
        value["futureTopLevel"] = true.into();
        value["providers"][0]["futureProvider"] = false.into();
        value["providers"][0]["quotas"][0]["futureQuota"] = "ignored".into();

        assert!(decode_feed_v1(&serde_json::to_vec(&value).unwrap()).is_ok());
    }
}
