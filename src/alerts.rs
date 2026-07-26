// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Pietrangelo Masala
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Alert configuration ────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    pub metric: AlertMetric,
    pub operator: AlertOperator,
    pub threshold: f32,
    pub severity: AlertSeverity,
    /// How long the condition must persist before firing (seconds).
    #[serde(default = "default_duration")]
    pub duration_secs: u64,
    /// Cooldown period between repeat notifications (seconds).
    #[serde(default)]
    pub cooldown_secs: u64,
    /// Optional mount point for disk alerts.
    #[serde(default)]
    pub mount_point: Option<String>,
    /// Whether this rule is currently enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_duration() -> u64 {
    0
}
fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertMetric {
    Cpu,
    Memory,
    Swap,
    Disk,
    Load1,
    Load5,
    Load15,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertOperator {
    Gt,
    Lt,
    Gte,
    Lte,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

// ── Fired alert ────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ActiveAlert {
    pub id: String,
    pub rule: AlertRule,
    pub current_value: f32,
    pub fired_at: String,
    pub message: String,
}

// ── Manager ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct AlertState {
    /// Timestamp (seconds) when the condition first became true.
    breached_since: Option<u64>,
    /// Timestamp (seconds) of last notification.
    last_notified: u64,
}

#[derive(Debug, Clone)]
pub struct AlertManager {
    pub rules: Vec<AlertRule>,
    pub active_alerts: Vec<ActiveAlert>,
    pub states: HashMap<String, AlertState>,
}

impl AlertManager {
    pub fn new(rules: Vec<AlertRule>) -> Self {
        Self {
            rules,
            active_alerts: Vec::new(),
            states: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(vec![
            AlertRule {
                metric: AlertMetric::Cpu,
                operator: AlertOperator::Gt,
                threshold: 90.0,
                severity: AlertSeverity::Warning,
                duration_secs: 60,
                cooldown_secs: 300,
                mount_point: None,
                enabled: true,
            },
            AlertRule {
                metric: AlertMetric::Cpu,
                operator: AlertOperator::Gt,
                threshold: 95.0,
                severity: AlertSeverity::Critical,
                duration_secs: 30,
                cooldown_secs: 300,
                mount_point: None,
                enabled: true,
            },
            AlertRule {
                metric: AlertMetric::Memory,
                operator: AlertOperator::Gt,
                threshold: 90.0,
                severity: AlertSeverity::Warning,
                duration_secs: 60,
                cooldown_secs: 300,
                mount_point: None,
                enabled: true,
            },
            AlertRule {
                metric: AlertMetric::Memory,
                operator: AlertOperator::Gt,
                threshold: 95.0,
                severity: AlertSeverity::Critical,
                duration_secs: 30,
                cooldown_secs: 300,
                mount_point: None,
                enabled: true,
            },
            AlertRule {
                metric: AlertMetric::Disk,
                operator: AlertOperator::Gt,
                threshold: 85.0,
                severity: AlertSeverity::Warning,
                duration_secs: 300,
                cooldown_secs: 600,
                mount_point: None,
                enabled: true,
            },
            AlertRule {
                metric: AlertMetric::Disk,
                operator: AlertOperator::Gt,
                threshold: 95.0,
                severity: AlertSeverity::Critical,
                duration_secs: 60,
                cooldown_secs: 600,
                mount_point: None,
                enabled: true,
            },
            AlertRule {
                metric: AlertMetric::Load5,
                operator: AlertOperator::Gt,
                threshold: 0.0,
                severity: AlertSeverity::Warning,
                duration_secs: 300,
                cooldown_secs: 600,
                mount_point: None,
                enabled: false,
            },
        ])
    }

    /// Evaluate all rules against current data. Returns newly-fired alerts.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate(
        &mut self,
        cpu_percent: f32,
        mem_percent: f32,
        swap_percent: f32,
        disk_usages: &HashMap<String, f32>,
        load1: f32,
        load5: f32,
        load15: f32,
        cpu_cores: usize,
        now_secs: u64,
    ) -> Vec<ActiveAlert> {
        let mut new_alerts = Vec::new();
        let mut updated_active = Vec::new();

        for (idx, rule) in self.rules.iter().enumerate() {
            if !rule.enabled {
                continue;
            }

            let current_value = match &rule.metric {
                AlertMetric::Cpu => cpu_percent,
                AlertMetric::Memory => mem_percent,
                AlertMetric::Swap => swap_percent,
                AlertMetric::Disk => {
                    if let Some(ref mp) = rule.mount_point {
                        *disk_usages.get(mp).unwrap_or(&0.0)
                    } else {
                        disk_usages.values().cloned().fold(0.0f32, f32::max)
                    }
                }
                AlertMetric::Load1 => load1,
                AlertMetric::Load5 => load5,
                AlertMetric::Load15 => load15,
            };

            // Handle auto-threshold for load (0 = auto = cpu_cores * 1.0)
            let threshold = if matches!(
                rule.metric,
                AlertMetric::Load1 | AlertMetric::Load5 | AlertMetric::Load15
            ) && rule.threshold == 0.0
            {
                cpu_cores as f32
            } else {
                rule.threshold
            };

            let breached = match rule.operator {
                AlertOperator::Gt => current_value > threshold,
                AlertOperator::Gte => current_value >= threshold,
                AlertOperator::Lt => current_value < threshold,
                AlertOperator::Lte => current_value <= threshold,
            };

            let rule_key = format!("rule_{}", idx);
            let state = self.states.entry(rule_key.clone()).or_insert(AlertState {
                breached_since: None,
                last_notified: 0,
            });

            if breached {
                if state.breached_since.is_none() {
                    state.breached_since = Some(now_secs);
                }

                let breach_duration = now_secs - state.breached_since.unwrap();
                let since_last = now_secs - state.last_notified;

                let mount_label = rule
                    .mount_point
                    .as_ref()
                    .map(|m| format!(" on {}", m))
                    .unwrap_or_default();

                if breach_duration >= rule.duration_secs && since_last >= rule.cooldown_secs {
                    state.last_notified = now_secs;

                    let alert = ActiveAlert {
                        id: uuid::Uuid::new_v4().to_string(),
                        rule: rule.clone(),
                        current_value,
                        fired_at: unix_to_iso8601(now_secs),
                        message: format!(
                            "{} {} {} {:.1}% (threshold: {:.1}%){} for {}s",
                            severity_name(&rule.severity),
                            metric_name(&rule.metric),
                            operator_symbol(&rule.operator),
                            current_value,
                            threshold,
                            mount_label,
                            breach_duration,
                        ),
                    };
                    new_alerts.push(alert.clone());
                    updated_active.push(alert);
                } else if breach_duration >= rule.duration_secs {
                    updated_active.push(ActiveAlert {
                        id: format!("ongoing_{}", rule_key),
                        rule: rule.clone(),
                        current_value,
                        fired_at: unix_to_iso8601(state.breached_since.unwrap()),
                        message: format!(
                            "{} {} {} {:.1}% (threshold: {:.1}%){} for {}s",
                            severity_name(&rule.severity),
                            metric_name(&rule.metric),
                            operator_symbol(&rule.operator),
                            current_value,
                            threshold,
                            mount_label,
                            breach_duration,
                        ),
                    });
                }
            } else {
                state.breached_since = None;
            }
        }

        self.active_alerts = updated_active;
        new_alerts
    }
}

fn metric_name(metric: &AlertMetric) -> &str {
    match metric {
        AlertMetric::Cpu => "CPU",
        AlertMetric::Memory => "Memory",
        AlertMetric::Swap => "Swap",
        AlertMetric::Disk => "Disk",
        AlertMetric::Load1 => "Load (1m)",
        AlertMetric::Load5 => "Load (5m)",
        AlertMetric::Load15 => "Load (15m)",
    }
}

pub fn severity_name(severity: &AlertSeverity) -> &str {
    match severity {
        AlertSeverity::Info => "INFO",
        AlertSeverity::Warning => "WARNING",
        AlertSeverity::Critical => "CRITICAL",
    }
}

fn operator_symbol(operator: &AlertOperator) -> &str {
    match operator {
        AlertOperator::Gt => ">",
        AlertOperator::Gte => ">=",
        AlertOperator::Lt => "<",
        AlertOperator::Lte => "<=",
    }
}

fn unix_to_iso8601(secs: u64) -> String {
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let mins = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    let mut y = 1970i64;
    let mut d = days_since_epoch as i64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if d < days_in_year {
            break;
        }
        d -= days_in_year;
        y += 1;
    }
    let month_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1;
    for &md in &month_days {
        if d < md as i64 {
            break;
        }
        d -= md as i64;
        m += 1;
    }
    let day = d + 1;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, day, hours, mins, s
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk_map(pairs: &[(&str, f32)]) -> HashMap<String, f32> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn with_defaults_has_seven_rules_and_load5_disabled() {
        let mgr = AlertManager::with_defaults();
        assert_eq!(mgr.rules.len(), 7);
        let load5 = mgr
            .rules
            .iter()
            .find(|r| r.metric == AlertMetric::Load5)
            .unwrap();
        assert!(!load5.enabled);
        assert!(mgr.active_alerts.is_empty());
        assert!(mgr.states.is_empty());
    }

    #[test]
    fn evaluate_no_rules_returns_nothing() {
        let mut mgr = AlertManager::new(vec![]);
        let alerts = mgr.evaluate(99.0, 99.0, 99.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 100);
        assert!(alerts.is_empty());
        assert!(mgr.active_alerts.is_empty());
    }

    #[test]
    fn evaluate_disabled_rule_is_skipped() {
        let rule = AlertRule {
            metric: AlertMetric::Cpu,
            operator: AlertOperator::Gt,
            threshold: 10.0,
            severity: AlertSeverity::Warning,
            duration_secs: 0,
            cooldown_secs: 0,
            mount_point: None,
            enabled: false,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        let alerts = mgr.evaluate(99.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 100);
        assert!(alerts.is_empty());
        assert!(mgr.active_alerts.is_empty());
    }

    #[test]
    fn evaluate_fires_immediately_when_duration_and_cooldown_are_zero() {
        let rule = AlertRule {
            metric: AlertMetric::Cpu,
            operator: AlertOperator::Gt,
            threshold: 90.0,
            severity: AlertSeverity::Warning,
            duration_secs: 0,
            cooldown_secs: 0,
            mount_point: None,
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        let alerts = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1000);
        assert_eq!(alerts.len(), 1);
        assert_eq!(mgr.active_alerts.len(), 1);
        assert!(alerts[0].message.contains("WARNING"));
        assert!(alerts[0].message.contains("CPU"));
        assert!(alerts[0].message.contains("95.0%"));
    }

    #[test]
    fn evaluate_does_not_fire_before_duration_elapsed() {
        let rule = AlertRule {
            metric: AlertMetric::Cpu,
            operator: AlertOperator::Gt,
            threshold: 90.0,
            severity: AlertSeverity::Warning,
            duration_secs: 60,
            cooldown_secs: 0,
            mount_point: None,
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        // First breach at t=1000: not enough duration yet.
        let alerts = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1000);
        assert!(alerts.is_empty());
        assert!(mgr.active_alerts.is_empty());

        // 30s later: still under duration.
        let alerts = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1030);
        assert!(alerts.is_empty());

        // 60s after the initial breach: duration satisfied, fires.
        let alerts = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1060);
        assert_eq!(alerts.len(), 1);
        assert_eq!(mgr.active_alerts.len(), 1);
    }

    #[test]
    fn evaluate_ongoing_alert_present_but_not_renotified_within_cooldown() {
        let rule = AlertRule {
            metric: AlertMetric::Cpu,
            operator: AlertOperator::Gt,
            threshold: 90.0,
            severity: AlertSeverity::Warning,
            duration_secs: 0,
            cooldown_secs: 300,
            mount_point: None,
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        let first = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1000);
        assert_eq!(first.len(), 1);

        // Still breached 100s later, within cooldown: no new alert, but still "active" (ongoing).
        let second = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1100);
        assert!(second.is_empty());
        assert_eq!(mgr.active_alerts.len(), 1);
        assert!(mgr.active_alerts[0].id.starts_with("ongoing_"));

        // After the cooldown elapses, it renotifies.
        let third = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1301);
        assert_eq!(third.len(), 1);
    }

    #[test]
    fn evaluate_clears_state_when_no_longer_breached() {
        let rule = AlertRule {
            metric: AlertMetric::Cpu,
            operator: AlertOperator::Gt,
            threshold: 90.0,
            severity: AlertSeverity::Warning,
            duration_secs: 60,
            cooldown_secs: 0,
            mount_point: None,
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1000);
        // Drops below threshold before duration elapses: breach resets.
        let alerts = mgr.evaluate(10.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1010);
        assert!(alerts.is_empty());
        assert!(mgr.active_alerts.is_empty());

        // Breaches again: duration timer restarted, not enough time elapsed yet.
        let alerts = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1011);
        assert!(alerts.is_empty());
    }

    #[test]
    fn evaluate_operators_lt_gte_lte() {
        for (op, value, threshold, expect_breach) in [
            (AlertOperator::Lt, 5.0, 10.0, true),
            (AlertOperator::Lt, 15.0, 10.0, false),
            (AlertOperator::Gte, 10.0, 10.0, true),
            (AlertOperator::Gte, 9.9, 10.0, false),
            (AlertOperator::Lte, 10.0, 10.0, true),
            (AlertOperator::Lte, 10.1, 10.0, false),
        ] {
            let rule = AlertRule {
                metric: AlertMetric::Memory,
                operator: op,
                threshold,
                severity: AlertSeverity::Info,
                duration_secs: 0,
                cooldown_secs: 0,
                mount_point: None,
                enabled: true,
            };
            let mut mgr = AlertManager::new(vec![rule]);
            let alerts = mgr.evaluate(0.0, value, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1);
            assert_eq!(
                !alerts.is_empty(),
                expect_breach,
                "value={value} threshold={threshold}"
            );
        }
    }

    #[test]
    fn evaluate_disk_uses_specific_mount_point() {
        let rule = AlertRule {
            metric: AlertMetric::Disk,
            operator: AlertOperator::Gt,
            threshold: 80.0,
            severity: AlertSeverity::Critical,
            duration_secs: 0,
            cooldown_secs: 0,
            mount_point: Some("/data".to_string()),
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        let disks = disk_map(&[("/", 99.0), ("/data", 50.0)]);
        // /data is under threshold even though / is over: mount-specific check wins.
        let alerts = mgr.evaluate(0.0, 0.0, 0.0, &disks, 0.0, 0.0, 0.0, 4, 1);
        assert!(alerts.is_empty());

        let disks = disk_map(&[("/", 10.0), ("/data", 90.0)]);
        let alerts = mgr.evaluate(0.0, 0.0, 0.0, &disks, 0.0, 0.0, 0.0, 4, 2);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("on /data"));
    }

    #[test]
    fn evaluate_disk_without_mount_point_uses_max_usage() {
        let rule = AlertRule {
            metric: AlertMetric::Disk,
            operator: AlertOperator::Gt,
            threshold: 80.0,
            severity: AlertSeverity::Critical,
            duration_secs: 0,
            cooldown_secs: 0,
            mount_point: None,
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        let disks = disk_map(&[("/", 10.0), ("/data", 90.0)]);
        let alerts = mgr.evaluate(0.0, 0.0, 0.0, &disks, 0.0, 0.0, 0.0, 4, 1);
        assert_eq!(alerts.len(), 1);
        assert!(!alerts[0].message.contains(" on "));
    }

    #[test]
    fn evaluate_disk_missing_mount_point_defaults_to_zero() {
        let rule = AlertRule {
            metric: AlertMetric::Disk,
            operator: AlertOperator::Gt,
            threshold: 1.0,
            severity: AlertSeverity::Warning,
            duration_secs: 0,
            cooldown_secs: 0,
            mount_point: Some("/missing".to_string()),
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        let disks = disk_map(&[("/", 99.0)]);
        let alerts = mgr.evaluate(0.0, 0.0, 0.0, &disks, 0.0, 0.0, 0.0, 4, 1);
        assert!(alerts.is_empty());
    }

    #[test]
    fn evaluate_load_auto_threshold_uses_cpu_cores() {
        let rule = AlertRule {
            metric: AlertMetric::Load5,
            operator: AlertOperator::Gt,
            threshold: 0.0, // 0 = auto
            severity: AlertSeverity::Warning,
            duration_secs: 0,
            cooldown_secs: 0,
            mount_point: None,
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        // load5 = 3.0, cpu_cores = 4 -> threshold becomes 4.0, not breached.
        let alerts = mgr.evaluate(0.0, 0.0, 0.0, &HashMap::new(), 0.0, 3.0, 0.0, 4, 1);
        assert!(alerts.is_empty());

        // load5 = 5.0 > 4 cores -> breached.
        let alerts = mgr.evaluate(0.0, 0.0, 0.0, &HashMap::new(), 0.0, 5.0, 0.0, 4, 2);
        assert_eq!(alerts.len(), 1);
    }

    #[test]
    fn evaluate_load_explicit_threshold_is_not_overridden() {
        let rule = AlertRule {
            metric: AlertMetric::Load1,
            operator: AlertOperator::Gt,
            threshold: 2.0,
            severity: AlertSeverity::Warning,
            duration_secs: 0,
            cooldown_secs: 0,
            mount_point: None,
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        // load1 = 3.0 > explicit threshold 2.0 (would NOT breach if auto-threshold with 8 cores).
        let alerts = mgr.evaluate(0.0, 0.0, 0.0, &HashMap::new(), 3.0, 0.0, 0.0, 8, 1);
        assert_eq!(alerts.len(), 1);
    }

    #[test]
    fn evaluate_swap_metric() {
        let rule = AlertRule {
            metric: AlertMetric::Swap,
            operator: AlertOperator::Gt,
            threshold: 50.0,
            severity: AlertSeverity::Warning,
            duration_secs: 0,
            cooldown_secs: 0,
            mount_point: None,
            enabled: true,
        };
        let mut mgr = AlertManager::new(vec![rule]);
        let alerts = mgr.evaluate(0.0, 0.0, 60.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("Swap"));
    }

    #[test]
    fn severity_name_and_operator_symbol_and_metric_name_cover_all_variants() {
        assert_eq!(severity_name(&AlertSeverity::Info), "INFO");
        assert_eq!(severity_name(&AlertSeverity::Warning), "WARNING");
        assert_eq!(severity_name(&AlertSeverity::Critical), "CRITICAL");

        assert_eq!(operator_symbol(&AlertOperator::Gt), ">");
        assert_eq!(operator_symbol(&AlertOperator::Gte), ">=");
        assert_eq!(operator_symbol(&AlertOperator::Lt), "<");
        assert_eq!(operator_symbol(&AlertOperator::Lte), "<=");

        assert_eq!(metric_name(&AlertMetric::Cpu), "CPU");
        assert_eq!(metric_name(&AlertMetric::Memory), "Memory");
        assert_eq!(metric_name(&AlertMetric::Swap), "Swap");
        assert_eq!(metric_name(&AlertMetric::Disk), "Disk");
        assert_eq!(metric_name(&AlertMetric::Load1), "Load (1m)");
        assert_eq!(metric_name(&AlertMetric::Load5), "Load (5m)");
        assert_eq!(metric_name(&AlertMetric::Load15), "Load (15m)");
    }

    #[test]
    fn unix_to_iso8601_epoch_zero() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn unix_to_iso8601_known_date() {
        // 2024-03-01T00:00:00Z (2024 is a leap year, so this exercises Feb 29 handling).
        assert_eq!(unix_to_iso8601(1_709_251_200), "2024-03-01T00:00:00Z");
    }

    #[test]
    fn unix_to_iso8601_with_time_of_day() {
        // 1970-01-01T01:02:03Z
        assert_eq!(unix_to_iso8601(3723), "1970-01-01T01:02:03Z");
    }

    #[test]
    fn is_leap_covers_gregorian_rules() {
        assert!(is_leap(2000)); // divisible by 400
        assert!(!is_leap(1900)); // divisible by 100 but not 400
        assert!(is_leap(2024)); // divisible by 4, not by 100
        assert!(!is_leap(2023));
    }

    #[test]
    fn alert_rule_deserializes_with_defaults() {
        let json = r#"{"metric":"cpu","operator":"gt","threshold":90.0,"severity":"warning"}"#;
        let rule: AlertRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.duration_secs, 0);
        assert_eq!(rule.cooldown_secs, 0);
        assert_eq!(rule.mount_point, None);
        assert!(rule.enabled);
    }

    #[test]
    fn alert_rule_deserializes_explicit_disabled() {
        let json = r#"{"metric":"disk","operator":"lte","threshold":5.0,"severity":"critical","enabled":false,"mount_point":"/data"}"#;
        let rule: AlertRule = serde_json::from_str(json).unwrap();
        assert!(!rule.enabled);
        assert_eq!(rule.mount_point.as_deref(), Some("/data"));
        assert_eq!(rule.operator, AlertOperator::Lte);
    }
}
