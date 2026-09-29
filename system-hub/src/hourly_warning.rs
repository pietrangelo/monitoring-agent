//! The hourly warning rule: a recurring problem of one system is logged at `warn` at most
//! once an hour, and at `debug` in between. Pure: the caller passes `now`, and keeps the time
//! of the last warning.

use std::time::{Duration, Instant};

/// How loudly to log one occurrence of a recurring problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HourlyWarning {
    /// The system's first occurrence in the last hour: logged at `warn`.
    Warn,
    /// Logged at `debug`.
    Quiet,
}

/// A system's recurring problem is logged at `warn` at most once an hour.
pub fn hourly_warning(last_warned: Option<Instant>, now: Instant) -> HourlyWarning {
    match last_warned {
        Some(at) if now.saturating_duration_since(at) < WARNING_EVERY => HourlyWarning::Quiet,
        _ => HourlyWarning::Warn,
    }
}

/// How often a system's recurring problem may be logged at `warn`.
const WARNING_EVERY: Duration = Duration::from_secs(3_600);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_systems_recurring_problem_warns_at_most_once_an_hour() {
        let base = Instant::now();
        let at = |secs: u64| base + Duration::from_secs(secs);
        let cases = [
            ("never warned", None, at(0), HourlyWarning::Warn),
            (
                "warned a minute ago",
                Some(at(0)),
                at(60),
                HourlyWarning::Quiet,
            ),
            (
                "just under an hour",
                Some(at(0)),
                at(3_599),
                HourlyWarning::Quiet,
            ),
            (
                "exactly an hour",
                Some(at(0)),
                at(3_600),
                HourlyWarning::Warn,
            ),
            (
                "a clock behind the last warning",
                Some(at(10)),
                at(0),
                HourlyWarning::Quiet,
            ),
        ];
        for (case, last, now, expected) in cases {
            assert_eq!(hourly_warning(last, now), expected, "case: {case}");
        }
    }
}
