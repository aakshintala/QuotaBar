use chrono::{DateTime, Duration, Utc};

use crate::feed::{ErrorCode, FeedDto, ProviderDto, QuotaDto};
use crate::quota::Severity;

pub const MAX_FEED_TEXT_CHARS: usize = 10_000;

/// The stable identity used to select bucket lines for prompt warnings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BucketKey {
    pub provider_id: String,
    pub key: String,
}

/// A recognized bucket severity whose complete line was included in the text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmittedBucketSeverity {
    pub provider_id: String,
    pub bucket_key: String,
    pub severity: Severity,
}

/// Human-readable quota context and only the bucket metadata it actually emits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormattedFeed {
    pub text: String,
    pub emitted_buckets: Vec<EmittedBucketSeverity>,
}

/// Render the v1 feed for a human-facing consumer. When `bucket_filter` is
/// present, render only matching bucket lines and their provider summaries; an
/// empty filter returns empty text. This is also the prompt-warning formatter.
pub fn format_feed(
    feed: &FeedDto,
    now: DateTime<Utc>,
    bucket_filter: Option<&[BucketKey]>,
) -> FormattedFeed {
    if bucket_filter.is_some_and(|filter| {
        !feed.providers.iter().any(|provider| {
            provider.quotas.iter().any(|quota| {
                recognized_severity(&quota.status).is_some()
                    && bucket_is_selected(provider, quota, filter)
            })
        })
    }) {
        return FormattedFeed {
            text: String::new(),
            emitted_buckets: Vec::new(),
        };
    }

    let mut output = String::new();
    let mut emitted_buckets = Vec::new();
    let mut truncated = false;
    append_line(&mut output, "Quota headroom (QuotaBar):");

    if feed.providers.is_empty()
        && !append_line(&mut output, "  No providers are reporting quota data.")
    {
        truncated = true;
    }

    for provider in &feed.providers {
        let selected_quotas: Vec<_> = provider
            .quotas
            .iter()
            .filter(|quota| {
                bucket_filter.is_none_or(|filter| bucket_is_selected(provider, quota, filter))
            })
            .collect();
        if bucket_filter.is_some() && selected_quotas.is_empty() {
            continue;
        }

        let summary = provider_summary(provider, now);
        if !append_line(&mut output, &summary) {
            truncated = true;
            break;
        }

        if let Some(error_code) = provider.error_code {
            let issue = format!("  Issue: {}", error_action(error_code));
            if !append_line(&mut output, &issue) {
                truncated = true;
                break;
            }
        }

        if selected_quotas.is_empty() && !append_line(&mut output, "  No current quota buckets.") {
            truncated = true;
            break;
        }

        for quota in selected_quotas {
            let Some(severity) = recognized_severity(&quota.status) else {
                continue;
            };
            let line = quota_line(quota, severity);
            if !append_line(&mut output, &line) {
                truncated = true;
                break;
            }
            emitted_buckets.push(EmittedBucketSeverity {
                provider_id: provider.id.clone(),
                bucket_key: quota.key.clone(),
                severity,
            });
        }
        if truncated {
            break;
        }
    }

    if !truncated && bucket_filter.is_none() && !feed.disabled_provider_ids.is_empty() {
        let disabled = disabled_line(&feed.disabled_provider_ids);
        if !append_line(&mut output, &disabled) {
            truncated = true;
        }
    }

    if truncated {
        append_line(&mut output, "… quota text truncated");
    }

    FormattedFeed {
        text: output,
        emitted_buckets,
    }
}

fn provider_summary(provider: &ProviderDto, now: DateTime<Utc>) -> String {
    let name = safe_fragment(&provider.name, 80);
    let id = safe_fragment(&provider.id, 80);
    let status = known_provider_status(&provider.status);
    let (freshness, age) = effective_freshness(provider, now);
    let suffix = match age {
        Some(age) => format!("{freshness}, {}", format_age(age)),
        None => freshness.to_owned(),
    };
    let no_data = if provider.quotas.is_empty() && status == "unknown" {
        "; no current quota data"
    } else {
        ""
    };
    format!("{name} ({id}): {status}; {suffix}{no_data}")
}

/// Use the capture timestamp rather than the serialized age/freshness, which
/// can be old by the time a consumer reads an otherwise successful HTTP reply.
fn effective_freshness(provider: &ProviderDto, now: DateTime<Utc>) -> (&'static str, Option<u64>) {
    let Some(captured_at) = provider.captured_at.as_deref() else {
        return ("unknown freshness", None);
    };
    let Ok(captured_at) = DateTime::parse_from_rfc3339(captured_at) else {
        return ("unknown freshness", None);
    };
    let captured_at = captured_at.with_timezone(&Utc);
    let age = now.signed_duration_since(captured_at);
    if age < Duration::zero() {
        return ("unknown freshness", None);
    }
    let seconds = age.num_seconds().max(0) as u64;
    let freshness = if seconds < 300 { "fresh" } else { "stale" };
    (freshness, Some(seconds))
}

fn format_age(seconds: u64) -> String {
    if seconds < 60 {
        "just now".to_owned()
    } else if seconds < 3_600 {
        format!("{}m old", seconds / 60)
    } else if seconds < 86_400 {
        let hours = seconds / 3_600;
        let minutes = (seconds % 3_600) / 60;
        if minutes == 0 {
            format!("{hours}h old")
        } else {
            format!("{hours}h{minutes}m old")
        }
    } else {
        let days = seconds / 86_400;
        let hours = (seconds % 86_400) / 3_600;
        if hours == 0 {
            format!("{days}d old")
        } else {
            format!("{days}d{hours}h old")
        }
    }
}

fn quota_line(quota: &QuotaDto, severity: Severity) -> String {
    let label = safe_fragment(&quota.label, 120);
    let mut details = Vec::new();
    if let Some(percent) = quota.percent_remaining {
        details.push(if percent.is_finite() {
            format!("{percent:.0}% remaining")
        } else {
            "unknown percentage remaining".to_owned()
        });
    }
    for (amount, kind) in [
        (quota.balance_remaining, "remaining"),
        (quota.balance_used, "used"),
        (quota.balance_cap, "cap"),
    ] {
        if let Some(amount) = amount {
            details.push(format_balance(amount, quota.balance_unit.as_deref(), kind));
        }
    }
    if let (Some(used), Some(limit)) = (quota.units_used, quota.units_limit) {
        details.push(format!("{used}/{limit} used"));
    }
    if details.is_empty() {
        details.push("no amount reported".to_owned());
    }

    let mut line = format!("  {label}: {} [{}]", details.join("; "), severity.as_str());
    if let Some(reset) = quota.reset_text.as_deref().or(quota.resets_at.as_deref()) {
        let reset = safe_fragment(reset, 80);
        if !reset.is_empty() {
            line.push_str(" · ");
            line.push_str(&reset);
        }
    }
    line
}

fn format_balance(amount: f64, unit: Option<&str>, kind: &str) -> String {
    if !amount.is_finite() {
        return format!("unknown balance {kind}");
    }
    let amount = format!("{amount:.2}");
    match unit {
        Some("usd") => format!("${amount} USD {kind}"),
        Some("credits") => format!("{amount} credits {kind}"),
        _ => format!("{amount} {kind}"),
    }
}

fn known_provider_status(status: &str) -> &'static str {
    match status {
        "unknown" => "unknown",
        "healthy" => "healthy",
        "warning" => "warning",
        "critical" => "critical",
        "depleted" => "depleted",
        _ => "unknown",
    }
}

fn recognized_severity(status: &str) -> Option<Severity> {
    match status {
        "healthy" => Some(Severity::Healthy),
        "warning" => Some(Severity::Warning),
        "critical" => Some(Severity::Critical),
        "depleted" => Some(Severity::Depleted),
        _ => None,
    }
}

fn bucket_is_selected(provider: &ProviderDto, quota: &QuotaDto, filter: &[BucketKey]) -> bool {
    filter
        .iter()
        .any(|key| key.provider_id == provider.id && key.key == quota.key)
}

fn error_action(error: ErrorCode) -> &'static str {
    match error {
        ErrorCode::CredentialsMissing => "no usable login; sign in with the provider CLI.",
        ErrorCode::LoginExpired => "login expired; renew the provider CLI login.",
        ErrorCode::KeychainBlocked => {
            "Keychain access was blocked; check access without prompting."
        }
        ErrorCode::SubscriptionUnavailable => "the provider reports no active subscription.",
        ErrorCode::Cooldown => "the provider is cooling down; retry after its guard deadline.",
        ErrorCode::RateLimited => {
            "the provider rate-limited the request; honor its retry deadline."
        }
        ErrorCode::NetworkError => "the request failed; check connectivity and retry.",
        ErrorCode::ConfigurationError => {
            "daemon configuration is invalid; correct ~/.quotabar/daemon.json."
        }
        ErrorCode::Busy => "the provider check is still in flight; retry shortly.",
        ErrorCode::ProviderError => "the provider check failed; inspect local daemon logs.",
        ErrorCode::PersistenceError => {
            "required state could not be saved; resolve the state error before retrying."
        }
        ErrorCode::InternalError => "an internal check failed; inspect local daemon logs.",
    }
}

fn safe_fragment(input: &str, max_chars: usize) -> String {
    input
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn disabled_line(ids: &[String]) -> String {
    const MAX_IDS: usize = 10;
    let mut displayed: Vec<_> = ids
        .iter()
        .take(MAX_IDS)
        .map(|id| safe_fragment(id, 80))
        .filter(|id| !id.is_empty())
        .collect();
    if ids.len() > MAX_IDS {
        displayed.push("…".to_owned());
    }
    format!("Disabled providers: {}", displayed.join(", "))
}

fn append_line(output: &mut String, line: &str) -> bool {
    let extra_chars = line.chars().count() + usize::from(!output.is_empty());
    if output.chars().count() + extra_chars > MAX_FEED_TEXT_CHARS {
        return false;
    }
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(line);
    true
}

#[cfg(test)]
mod tests {
    use super::{BucketKey, MAX_FEED_TEXT_CHARS, format_feed};
    use crate::feed::{ErrorCode, FeedDto, ProviderDto, QuotaDto};
    use crate::quota::Severity;
    use chrono::{DateTime, Duration, Utc};

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(seconds, 0).unwrap()
    }

    fn quota(key: &str, label: &str, status: &str) -> QuotaDto {
        QuotaDto {
            key: key.to_owned(),
            label: label.to_owned(),
            percent_remaining: None,
            balance_remaining: None,
            balance_used: None,
            balance_cap: None,
            balance_unit: None,
            units_used: None,
            units_limit: None,
            resets_at: None,
            reset_text: None,
            status: status.to_owned(),
        }
    }

    fn provider(
        id: &str,
        captured_at: Option<DateTime<Utc>>,
        quotas: Vec<QuotaDto>,
    ) -> ProviderDto {
        ProviderDto {
            id: id.to_owned(),
            name: id.to_owned(),
            tier: None,
            captured_at: captured_at.map(|value| value.to_rfc3339()),
            age_seconds: None,
            freshness: "fresh".to_owned(),
            error_code: None,
            status: if quotas.is_empty() {
                "unknown".to_owned()
            } else {
                "healthy".to_owned()
            },
            quotas,
        }
    }

    fn feed(providers: Vec<ProviderDto>) -> FeedDto {
        FeedDto {
            schema_version: 1,
            generated_at: "2027-01-15T08:00:00Z".to_owned(),
            disabled_provider_ids: Vec::new(),
            providers,
        }
    }

    #[test]
    fn formats_percent_reset_and_only_records_recognized_complete_bucket_metadata() {
        let now = time(1_800_000_000);
        let mut percent = quota("session", "Session", "critical");
        percent.percent_remaining = Some(17.5);
        percent.reset_text = Some("Resets in 2h".to_owned());
        let unknown = quota("future", "Future", "provider_specific");
        let result = format_feed(
            &feed(vec![provider("claude", Some(now), vec![percent, unknown])]),
            now,
            None,
        );

        assert!(
            result
                .text
                .contains("claude (claude): healthy; fresh, just now")
        );
        assert!(
            result
                .text
                .contains("Session: 18% remaining [critical] · Resets in 2h")
        );
        assert!(!result.text.contains("provider_specific"));
        assert_eq!(
            result.emitted_buckets,
            [super::EmittedBucketSeverity {
                provider_id: "claude".to_owned(),
                bucket_key: "session".to_owned(),
                severity: Severity::Critical,
            }]
        );
    }

    #[test]
    fn formats_usd_credit_count_and_reset_fallback_fields() {
        let now = time(1_800_000_000);
        let mut usd = quota("usd", "API spend", "healthy");
        usd.balance_remaining = Some(1.25);
        usd.balance_used = Some(3.5);
        usd.balance_cap = Some(4.75);
        usd.balance_unit = Some("usd".to_owned());
        let mut credits = quota("credits", "Credits", "warning");
        credits.balance_remaining = Some(12.0);
        credits.balance_unit = Some("credits".to_owned());
        credits.resets_at = Some("2027-01-15T10:00:00Z".to_owned());
        let mut count = quota("requests", "Requests", "critical");
        count.units_used = Some(90);
        count.units_limit = Some(100);

        let result = format_feed(
            &feed(vec![provider(
                "codex",
                Some(now),
                vec![usd, credits, count],
            )]),
            now,
            None,
        );
        assert!(result.text.contains("$1.25 USD remaining"));
        assert!(result.text.contains("$3.50 USD used"));
        assert!(result.text.contains("$4.75 USD cap"));
        assert!(result.text.contains("12.00 credits remaining"));
        assert!(result.text.contains("90/100 used"));
        assert!(result.text.contains("2027-01-15T10:00:00Z"));
    }

    #[test]
    fn reports_staleness_from_capture_time_and_safe_unknown_busy_error_summaries() {
        let now = time(1_800_000_000);
        let stale = provider(
            "cursor",
            Some(now - Duration::hours(2)),
            vec![quota("usage", "Usage", "healthy")],
        );
        let mut unknown = provider("muse", None, Vec::new());
        unknown.error_code = Some(ErrorCode::CredentialsMissing);
        let mut busy = provider("codex", None, Vec::new());
        busy.error_code = Some(ErrorCode::Busy);
        let result = format_feed(&feed(vec![stale, unknown, busy]), now, None);

        assert!(
            result
                .text
                .contains("cursor (cursor): healthy; stale, 2h old")
        );
        assert!(result.text.contains("no current quota data"));
        assert!(result.text.contains("sign in with the provider CLI"));
        assert!(result.text.contains("check is still in flight"));
        assert_eq!(result.emitted_buckets.len(), 1);
    }

    #[test]
    fn filtered_format_uses_same_renderer_and_only_emits_selected_bucket_keys() {
        let now = time(1_800_000_000);
        let mut warning = quota("weekly", "Weekly", "warning");
        warning.percent_remaining = Some(40.0);
        let mut healthy = quota("session", "Session", "healthy");
        healthy.percent_remaining = Some(80.0);
        let feed = feed(vec![provider(
            "opencode-go",
            Some(now),
            vec![warning, healthy],
        )]);
        let filter = [BucketKey {
            provider_id: "opencode-go".to_owned(),
            key: "weekly".to_owned(),
        }];

        let selected = format_feed(&feed, now, Some(&filter));
        assert!(selected.text.contains("Weekly: 40% remaining [warning]"));
        assert!(!selected.text.contains("Session:"));
        assert_eq!(selected.emitted_buckets.len(), 1);
        assert_eq!(selected.emitted_buckets[0].bucket_key, "weekly");
        assert_eq!(format_feed(&feed, now, Some(&[])).text, "");
    }

    #[test]
    fn empty_feed_has_compact_human_text_and_no_bucket_metadata() {
        let result = format_feed(&feed(Vec::new()), time(1_800_000_000), None);
        assert!(
            result
                .text
                .contains("No providers are reporting quota data")
        );
        assert!(result.emitted_buckets.is_empty());
    }

    #[test]
    fn output_is_unicode_bounded_and_truncated_bucket_is_not_reported_emitted() {
        let now = time(1_800_000_000);
        let quotas = (0..160)
            .map(|index| {
                let mut quota = quota(
                    &format!("key-{index}"),
                    &format!("😀{index}-{}", "x".repeat(100)),
                    "warning",
                );
                quota.percent_remaining = Some(40.0);
                quota
            })
            .collect();
        let result = format_feed(&feed(vec![provider("muse", Some(now), quotas)]), now, None);

        assert!(result.text.chars().count() <= MAX_FEED_TEXT_CHARS);
        assert!(result.text.is_char_boundary(result.text.len()));
        assert!(result.text.ends_with("… quota text truncated"));
        assert!(!result.emitted_buckets.is_empty());
        assert!(result.emitted_buckets.len() < 160);
        let last_emitted = &result.emitted_buckets.last().unwrap().bucket_key;
        let next_index: usize = last_emitted
            .trim_start_matches("key-")
            .parse::<usize>()
            .unwrap()
            + 1;
        assert!(!result.text.contains(&format!("😀{next_index}-")));
        assert!(
            result
                .emitted_buckets
                .iter()
                .all(|bucket| bucket.severity == Severity::Warning)
        );
    }

    #[test]
    fn unsafe_control_characters_cannot_inject_lines_or_error_text() {
        let now = time(1_800_000_000);
        let mut provider = provider(
            "cursor\n\u{1b}[31msecret",
            Some(now),
            vec![quota("key", "Usage\nInjected", "healthy")],
        );
        provider.name = "Cursor\r\nInjected".to_owned();
        provider.error_code = Some(ErrorCode::LoginExpired);
        let result = format_feed(&feed(vec![provider]), now, None);

        assert!(
            result
                .text
                .chars()
                .all(|character| !character.is_control() || character == '\n')
        );
        assert!(!result.text.contains("Injected\n"));
        assert!(
            result
                .text
                .contains("login expired; renew the provider CLI login")
        );
    }
}
