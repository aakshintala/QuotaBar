use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::quota::{Observation, Quota};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenCodeError {
    MalformedOrIncomplete,
    InvalidTimestamp,
    InvalidPercentage,
}

#[derive(Deserialize)]
struct Response {
    usage: Usage,
}

#[derive(Deserialize)]
struct Usage {
    rolling: Window,
    weekly: Window,
    monthly: Window,
}

#[derive(Deserialize)]
struct Window {
    percent: f64,
    #[serde(rename = "resetsAt")]
    resets_at: serde_json::Value,
}

/// Decode OpenCode's server-side used-percent response into a shared observation.
/// Error variants are deliberately safe and never retain provider response text.
pub fn parse_usage(input: &[u8], captured_at: DateTime<Utc>) -> Result<Observation, OpenCodeError> {
    let response: Response =
        serde_json::from_slice(input).map_err(|_| OpenCodeError::MalformedOrIncomplete)?;

    let rolling = quota(
        response.usage.rolling,
        "session",
        "Session",
        Some(5 * 60 * 60),
    )?;
    let weekly = quota(
        response.usage.weekly,
        "weekly",
        "Weekly",
        Some(7 * 24 * 60 * 60),
    )?;
    let monthly = quota(
        response.usage.monthly,
        "time:Monthly",
        "Monthly",
        Some(30 * 24 * 60 * 60),
    )?;

    Ok(Observation {
        captured_at,
        tier: None,
        quotas: vec![rolling, weekly, monthly],
    })
}

fn quota(
    window: Window,
    key: &str,
    label: &str,
    window_duration_seconds: Option<i64>,
) -> Result<Quota, OpenCodeError> {
    if !window.percent.is_finite() {
        return Err(OpenCodeError::InvalidPercentage);
    }
    let resets_at = match window.resets_at {
        serde_json::Value::Null => None,
        serde_json::Value::String(value) => Some(
            DateTime::parse_from_rfc3339(&value)
                .map_err(|_| OpenCodeError::InvalidTimestamp)?
                .with_timezone(&Utc),
        ),
        _ => return Err(OpenCodeError::MalformedOrIncomplete),
    };

    Ok(Quota {
        key: key.to_owned(),
        label: label.to_owned(),
        percent_remaining: Some(100.0 - window.percent.clamp(0.0, 100.0)),
        balance_remaining: None,
        balance_used: None,
        balance_cap: None,
        balance_unit: None,
        units_used: None,
        units_limit: None,
        resets_at,
        window_duration_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::{OpenCodeError, parse_usage};
    use crate::quota::Observation;
    use chrono::{DateTime, Utc};

    const EMPTY_ZERO_NULL: &str =
        include_str!("../../../Tests/Fixtures/OpenCode/empty-zero-null.json");
    const OVER_LIMIT: &str = include_str!("../../../Tests/Fixtures/OpenCode/over-limit.json");
    const THREE_WINDOWS: &str = include_str!("../../../Tests/Fixtures/OpenCode/three-windows.json");

    fn captured_at() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn parse(source: &str) -> Result<Observation, OpenCodeError> {
        parse_usage(source.as_bytes(), captured_at())
    }

    #[test]
    fn parses_all_three_shared_synthetic_fixtures() {
        let empty = parse(EMPTY_ZERO_NULL).unwrap();
        assert_eq!(empty.quotas.len(), 3);
        assert!(
            empty
                .quotas
                .iter()
                .all(|quota| quota.percent_remaining == Some(100.0))
        );
        assert!(empty.quotas.iter().all(|quota| quota.resets_at.is_none()));

        let over = parse(OVER_LIMIT).unwrap();
        assert_eq!(over.quotas[0].percent_remaining, Some(0.0));
        assert_eq!(over.quotas[1].percent_remaining, Some(50.0));
        assert_eq!(over.quotas[0].key, "session");
        assert_eq!(over.quotas[1].key, "weekly");
        assert_eq!(over.quotas[2].key, "time:Monthly");
        assert_eq!(
            over.quotas
                .iter()
                .map(|quota| quota.label.as_str())
                .collect::<Vec<_>>(),
            ["Session", "Weekly", "Monthly"]
        );
        assert_eq!(over.quotas[0].window_duration_seconds, Some(5 * 60 * 60));
        assert_eq!(
            over.quotas[1].window_duration_seconds,
            Some(7 * 24 * 60 * 60)
        );
        assert_eq!(
            over.quotas[2].window_duration_seconds,
            Some(30 * 24 * 60 * 60)
        );
        assert!(over.quotas.iter().all(|quota| quota.resets_at.is_some()));

        let three = parse(THREE_WINDOWS).unwrap();
        assert!((three.quotas[0].percent_remaining.unwrap() - 79.16667).abs() < 0.00001);
        assert_eq!(three.quotas[1].percent_remaining, Some(75.0));
        assert_eq!(three.quotas[2].percent_remaining, Some(75.0));
        assert_eq!(three.captured_at, captured_at());
    }

    #[test]
    fn clamps_used_percent_before_converting_to_remaining() {
        let json = r#"{"usage":{"rolling":{"percent":-20,"resetsAt":null},"weekly":{"percent":140,"resetsAt":null},"monthly":{"percent":50,"resetsAt":null}}}"#;
        let observation = parse(json).unwrap();
        assert_eq!(observation.quotas[0].percent_remaining, Some(100.0));
        assert_eq!(observation.quotas[1].percent_remaining, Some(0.0));
        assert_eq!(observation.quotas[2].percent_remaining, Some(50.0));
    }

    #[test]
    fn rejects_malformed_or_incomplete_responses_without_body_text() {
        assert_eq!(parse("not-json"), Err(OpenCodeError::MalformedOrIncomplete));
        let missing_monthly = r#"{"usage":{"rolling":{"percent":1,"resetsAt":null},"weekly":{"percent":1,"resetsAt":null}}}"#;
        assert_eq!(
            parse(missing_monthly),
            Err(OpenCodeError::MalformedOrIncomplete)
        );
        let missing_reset_field = r#"{"usage":{"rolling":{"percent":1},"weekly":{"percent":1,"resetsAt":null},"monthly":{"percent":1,"resetsAt":null}}}"#;
        assert_eq!(
            parse(missing_reset_field),
            Err(OpenCodeError::MalformedOrIncomplete)
        );
    }

    #[test]
    fn rejects_invalid_timestamps_and_non_finite_percentages() {
        let invalid_timestamp = r#"{"usage":{"rolling":{"percent":1,"resetsAt":"tomorrow"},"weekly":{"percent":1,"resetsAt":null},"monthly":{"percent":1,"resetsAt":null}}}"#;
        assert_eq!(
            parse(invalid_timestamp),
            Err(OpenCodeError::InvalidTimestamp)
        );
        let non_finite = r#"{"usage":{"rolling":{"percent":1e999,"resetsAt":null},"weekly":{"percent":1,"resetsAt":null},"monthly":{"percent":1,"resetsAt":null}}}"#;
        assert!(matches!(
            parse(non_finite),
            Err(OpenCodeError::MalformedOrIncomplete | OpenCodeError::InvalidPercentage)
        ));
    }
}
