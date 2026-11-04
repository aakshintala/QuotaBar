use chrono::{DateTime, Utc};

/// One successfully observed provider state. Capture time is set by the caller
/// only after the provider response has been parsed successfully.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub captured_at: DateTime<Utc>,
    pub tier: Option<String>,
    pub quotas: Vec<Quota>,
}

/// The smallest common representation for percent, balance, and count meters.
#[derive(Clone, Debug, PartialEq)]
pub struct Quota {
    pub key: String,
    pub label: String,
    pub percent_remaining: Option<f64>,
    pub balance_remaining: Option<f64>,
    pub balance_used: Option<f64>,
    pub balance_cap: Option<f64>,
    pub balance_unit: Option<String>,
    pub units_used: Option<i64>,
    pub units_limit: Option<i64>,
    pub resets_at: Option<DateTime<Utc>>,
    /// A duration is set only when the window has meaningful pace semantics.
    pub window_duration_seconds: Option<i64>,
}

impl Quota {
    pub fn status(&self, now: &DateTime<Utc>) -> Severity {
        let percent =
            self.percent_remaining
                .or_else(|| match (self.units_used, self.units_limit) {
                    (Some(used), Some(limit)) if limit > 0 => {
                        Some((limit as f64 - used as f64) / limit as f64 * 100.0)
                    }
                    _ => None,
                });
        let Some(percent) = percent else {
            // Balance-only meters have no meaningful percentage and are healthy.
            return Severity::Healthy;
        };

        if percent <= 0.0 {
            return Severity::Depleted;
        }

        let elapsed_percent = self.resets_at.as_ref().and_then(|resets_at| {
            let duration = self.window_duration_seconds?;
            if duration <= 0 {
                return None;
            }
            let until_reset = resets_at.signed_duration_since(*now);
            let elapsed = duration as f64 - until_reset.as_seconds_f64();
            Some((elapsed / duration as f64 * 100.0).clamp(0.0, 100.0))
        });

        match elapsed_percent {
            Some(elapsed) if elapsed > 0.0 => {
                let burn_rate = (100.0 - percent) / elapsed;
                if percent < 20.0 {
                    if burn_rate > 1.0 {
                        Severity::Critical
                    } else {
                        Severity::Healthy
                    }
                } else if burn_rate > 1.5 && percent < 50.0 {
                    Severity::Warning
                } else {
                    Severity::Healthy
                }
            }
            _ => Severity::from_percent(percent),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    Healthy,
    Warning,
    Critical,
    Depleted,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Warning => "warning",
            Self::Critical => "critical",
            Self::Depleted => "depleted",
        }
    }

    fn from_percent(percent: f64) -> Self {
        if percent <= 0.0 {
            Self::Depleted
        } else if percent < 20.0 {
            Self::Critical
        } else if percent < 50.0 {
            Self::Warning
        } else {
            Self::Healthy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Quota, Severity};
    use chrono::{DateTime, Duration, Utc};

    fn percent_quota(
        percent: f64,
        resets_at: Option<DateTime<Utc>>,
        duration: Option<i64>,
    ) -> Quota {
        Quota {
            key: "test".into(),
            label: "Test".into(),
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

    #[test]
    fn absolute_thresholds_apply_without_meaningful_window_duration() {
        let now = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap();
        assert_eq!(
            percent_quota(-2.0, None, None).status(&now),
            Severity::Depleted
        );
        assert_eq!(
            percent_quota(10.0, None, None).status(&now),
            Severity::Critical
        );
        assert_eq!(
            percent_quota(30.0, None, None).status(&now),
            Severity::Warning
        );
        assert_eq!(
            percent_quota(75.0, None, None).status(&now),
            Severity::Healthy
        );
    }

    #[test]
    fn meaningful_window_uses_response_time_pace_rules() {
        let now = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap();
        let reset = now + Duration::hours(2);
        assert_eq!(
            percent_quota(10.0, Some(reset), Some(5 * 3600)).status(&now),
            Severity::Critical
        );
        assert_eq!(
            percent_quota(10.0, Some(now + Duration::minutes(10)), Some(5 * 3600)).status(&now),
            Severity::Healthy
        );
        assert_eq!(
            percent_quota(
                40.0,
                Some(now + Duration::hours(3) + Duration::minutes(30)),
                Some(5 * 3600)
            )
            .status(&now),
            Severity::Warning
        );
    }

    #[test]
    fn count_meter_can_derive_severity_without_serializing_a_percent() {
        let now = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let mut quota = percent_quota(100.0, None, None);
        quota.percent_remaining = None;
        quota.units_used = Some(90);
        quota.units_limit = Some(100);
        assert_eq!(quota.status(&now), Severity::Critical);
    }

    #[test]
    fn balance_only_meter_is_healthy() {
        let now = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let mut quota = percent_quota(0.0, None, None);
        quota.percent_remaining = None;
        quota.balance_remaining = Some(0.0);
        quota.balance_unit = Some("usd".into());
        assert_eq!(quota.status(&now), Severity::Healthy);
    }
}
