//! The `Nd|Nh|Nm` deployment-expiration duration grammar, and the pure rule
//! that caps a deployment's `expires_at` against its environment's
//! `max_deployment_expiration`.
//!
//! Two callers share this grammar: deployment creation parses a caller-supplied
//! `expires_in` (e.g. `--expire 7d`) into an absolute deadline, and environment
//! validation parses `max_deployment_expiration` before it is stored. Keeping
//! one parser means the two can never drift on what counts as valid.

use chrono::{DateTime, Utc};
use std::fmt;

use crate::models::Environment;

/// A parsed `Nd|Nh|Nm` duration, e.g. `7d`, `12h`, `30m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpirationDuration {
    pub count: i64,
    pub unit: ExpirationUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpirationUnit {
    Days,
    Hours,
    Minutes,
}

impl ExpirationDuration {
    /// Parse a duration string such as `"7d"`, `"12h"` or `"30m"`. Leading and
    /// trailing whitespace is ignored, so `" 07d "` parses the same as `"7d"` —
    /// [`Display`](fmt::Display) then renders it back in canonical form.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (num_str, unit) = if let Some(num_str) = s.strip_suffix('d') {
            (num_str, ExpirationUnit::Days)
        } else if let Some(num_str) = s.strip_suffix('h') {
            (num_str, ExpirationUnit::Hours)
        } else if let Some(num_str) = s.strip_suffix('m') {
            (num_str, ExpirationUnit::Minutes)
        } else {
            return Err("Duration must end with d, h, or m".to_string());
        };

        let count: i64 = num_str
            .parse()
            .map_err(|_| "Invalid number in duration".to_string())?;

        if count <= 0 {
            return Err("Duration must be positive".to_string());
        }

        Ok(Self { count, unit })
    }

    /// Convert to a [`chrono::Duration`]. Errs on overflow rather than
    /// panicking, unlike `chrono::Duration::days`/`hours`/`minutes`.
    pub fn to_duration(self) -> Result<chrono::Duration, String> {
        match self.unit {
            ExpirationUnit::Days => chrono::Duration::try_days(self.count),
            ExpirationUnit::Hours => chrono::Duration::try_hours(self.count),
            ExpirationUnit::Minutes => chrono::Duration::try_minutes(self.count),
        }
        .ok_or_else(|| "Duration is too large".to_string())
    }
}

impl fmt::Display for ExpirationDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = match self.unit {
            ExpirationUnit::Days => 'd',
            ExpirationUnit::Hours => 'h',
            ExpirationUnit::Minutes => 'm',
        };
        write!(f, "{}{}", self.count, unit)
    }
}

/// Detail recorded on a deployment's creation event when its `expires_at` was
/// capped by its environment's `max_deployment_expiration`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpirationCap {
    /// What the caller asked for (a parsed `--expire`), if anything. `None`
    /// when the deployment had no requested expiration at all — the cap is
    /// then the sole source of `expires_at`.
    pub requested_expires_at: Option<DateTime<Utc>>,
    /// The environment's `max_deployment_expiration`, in canonical form.
    pub max_deployment_expiration: String,
    /// Name of the environment that imposed the cap.
    pub environment: String,
}

/// Decide the `expires_at` a new deployment should carry, applying the
/// environment's `max_deployment_expiration` when it applies.
///
/// The cap applies only to deployments created into a group other than the
/// environment's `primary_deployment_group` — an environment with no primary
/// group has no group exempt from it. `requested` is the caller's parsed
/// `--expire`, if any; `now` is the creation time the deadline is computed
/// from.
///
/// Returns the deployment's final `expires_at` alongside an [`ExpirationCap`]
/// when the cap was the reason (or part of the reason) for that value — `None`
/// when `requested` passed through unmodified. Errs only when the stored
/// `max_deployment_expiration` fails to parse, which means the database and
/// this parser have disagreed — the CHECK constraint on the column should have
/// made that impossible.
pub fn effective_expiration(
    requested: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    env: Option<&Environment>,
    group: &str,
) -> Result<(Option<DateTime<Utc>>, Option<ExpirationCap>), String> {
    let Some(env) = env else {
        return Ok((requested, None));
    };
    let Some(max) = env.max_deployment_expiration.as_deref() else {
        return Ok((requested, None));
    };
    if env.primary_deployment_group.as_deref() == Some(group) {
        return Ok((requested, None));
    }

    let duration = ExpirationDuration::parse(max)?.to_duration()?;
    let cap_at = now
        .checked_add_signed(duration)
        .ok_or_else(|| "max_deployment_expiration is too large".to_string())?;

    if requested.is_none_or(|r| r > cap_at) {
        Ok((
            Some(cap_at),
            Some(ExpirationCap {
                requested_expires_at: requested,
                max_deployment_expiration: max.to_string(),
                environment: env.name.clone(),
            }),
        ))
    } else {
        Ok((requested, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_environment(primary_group: Option<&str>, max: Option<&str>) -> Environment {
        Environment {
            id: Uuid::nil(),
            project_id: Uuid::nil(),
            name: "staging".to_string(),
            primary_deployment_group: primary_group.map(str::to_string),
            is_production: false,
            color: "green".to_string(),
            min_replicas: None,
            max_replicas: None,
            min_cpu: None,
            max_cpu: None,
            min_memory: None,
            max_memory: None,
            max_deployment_expiration: max.map(str::to_string),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn parse_and_canonical_round_trip() {
        for (input, expected) in [
            ("7d", "7d"),
            (" 12h ", "12h"),
            ("30m", "30m"),
            ("07d", "7d"),
        ] {
            let parsed = ExpirationDuration::parse(input).unwrap();
            assert_eq!(parsed.to_string(), expected);
        }
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(ExpirationDuration::parse("7").is_err());
        assert!(ExpirationDuration::parse("7w").is_err());
        assert!(ExpirationDuration::parse("0d").is_err());
        assert!(ExpirationDuration::parse("-1d").is_err());
        assert!(ExpirationDuration::parse("xd").is_err());
    }

    #[test]
    fn to_duration_overflows_to_err_instead_of_panicking() {
        let huge = ExpirationDuration {
            count: i64::MAX,
            unit: ExpirationUnit::Days,
        };
        assert!(huge.to_duration().is_err());
    }

    #[test]
    fn primary_group_deployments_are_untouched() {
        let env = test_environment(Some("default"), Some("7d"));
        let now = Utc::now();
        let requested = Some(now + chrono::Duration::days(365));
        let (expires_at, cap) =
            effective_expiration(requested, now, Some(&env), "default").unwrap();
        assert_eq!(expires_at, requested);
        assert!(cap.is_none());
    }

    #[test]
    fn non_primary_group_without_request_gets_max_as_deadline() {
        let env = test_environment(Some("default"), Some("7d"));
        let now = Utc::now();
        let (expires_at, cap) = effective_expiration(None, now, Some(&env), "mr/123").unwrap();
        assert_eq!(expires_at, Some(now + chrono::Duration::days(7)));
        let cap = cap.unwrap();
        assert_eq!(cap.requested_expires_at, None);
        assert_eq!(cap.max_deployment_expiration, "7d");
        assert_eq!(cap.environment, "staging");
    }

    #[test]
    fn non_primary_group_under_max_is_untouched() {
        let env = test_environment(Some("default"), Some("7d"));
        let now = Utc::now();
        let requested = Some(now + chrono::Duration::days(1));
        let (expires_at, cap) = effective_expiration(requested, now, Some(&env), "mr/123").unwrap();
        assert_eq!(expires_at, requested);
        assert!(cap.is_none());
    }

    #[test]
    fn non_primary_group_over_max_is_clamped() {
        let env = test_environment(Some("default"), Some("7d"));
        let now = Utc::now();
        let requested = Some(now + chrono::Duration::days(30));
        let (expires_at, cap) = effective_expiration(requested, now, Some(&env), "mr/123").unwrap();
        assert_eq!(expires_at, Some(now + chrono::Duration::days(7)));
        let cap = cap.unwrap();
        assert_eq!(cap.requested_expires_at, requested);
        assert_eq!(cap.max_deployment_expiration, "7d");
    }

    #[test]
    fn env_without_primary_group_caps_every_group() {
        let env = test_environment(None, Some("7d"));
        let now = Utc::now();
        let (expires_at, cap) = effective_expiration(None, now, Some(&env), "default").unwrap();
        assert_eq!(expires_at, Some(now + chrono::Duration::days(7)));
        assert!(cap.is_some());
    }

    #[test]
    fn env_without_max_is_untouched() {
        let env = test_environment(Some("default"), None);
        let now = Utc::now();
        let requested = Some(now + chrono::Duration::days(365));
        let (expires_at, cap) = effective_expiration(requested, now, Some(&env), "mr/123").unwrap();
        assert_eq!(expires_at, requested);
        assert!(cap.is_none());
    }

    #[test]
    fn no_environment_is_untouched() {
        let now = Utc::now();
        let requested = Some(now + chrono::Duration::days(365));
        let (expires_at, cap) = effective_expiration(requested, now, None, "mr/123").unwrap();
        assert_eq!(expires_at, requested);
        assert!(cap.is_none());
    }
}
