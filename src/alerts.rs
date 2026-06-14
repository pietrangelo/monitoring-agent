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
