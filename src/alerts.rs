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
use std::fmt;
use uuid::Uuid;

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

// ── Incidents ──────────────────────────────────────────

/// One lifetime of the agent process. Minted once at startup, so incident ids from
/// different runs of the same agent never meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentRun(Uuid);

impl AgentRun {
    pub fn new(id: Uuid) -> Self {
        Self(id)
    }
}

/// Identifies one alert incident: the agent run that saw it and its place in that run's
/// sequence of incidents, starting at 1.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IncidentId {
    run: AgentRun,
    sequence: u64,
}

impl fmt::Display for IncidentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.run.0.hyphenated(), self.sequence)
    }
}

/// Mints the incident ids of one agent run. It never rewinds, not even when the rule set is
/// replaced, so no two incidents of a run share an id.
#[derive(Debug)]
struct IncidentSequence {
    run: AgentRun,
    minted: u64,
}

impl IncidentSequence {
    fn mint(&mut self) -> IncidentId {
        self.minted += 1;
        IncidentId {
            run: self.run,
            sequence: self.minted,
        }
    }
}

/// An alert incident that has become active.
#[derive(Debug)]
struct Incident {
    id: IncidentId,
    activated_at: u64,
}

/// Where one rule stands on its current breach.
#[derive(Debug, Default)]
enum Breach {
    #[default]
    Clear,
    /// Breaching, but not yet for the rule's duration.
    Pending {
        since: u64,
    },
    Active {
        since: u64,
        incident: Incident,
    },
}

impl Breach {
    /// The state after one tick. A breach start later than `now` means the clock stepped
    /// back: it moves back to `now`, so the wait restarts on the new clock instead of
    /// stalling until the clock catches up.
    fn advance(
        self,
        breached: bool,
        duration_secs: u64,
        now: u64,
        incidents: &mut IncidentSequence,
    ) -> Self {
        if !breached {
            return Breach::Clear;
        }
        match self {
            Breach::Clear => Self::pending_or_active(now, duration_secs, now, incidents),
            Breach::Pending { since } => {
                Self::pending_or_active(since.min(now), duration_secs, now, incidents)
            }
            Breach::Active { since, incident } => Breach::Active {
                since: since.min(now),
                incident,
            },
        }
    }

    fn pending_or_active(
        since: u64,
        duration_secs: u64,
        now: u64,
        incidents: &mut IncidentSequence,
    ) -> Self {
        if now.saturating_sub(since) < duration_secs {
            return Breach::Pending { since };
        }
        let incident = Incident {
            id: incidents.mint(),
            activated_at: now,
        };
        Breach::Active { since, incident }
    }

    fn active_incident(&self) -> Option<(u64, &Incident)> {
        match self {
            Breach::Clear | Breach::Pending { .. } => None,
            Breach::Active { since, incident } => Some((*since, incident)),
        }
    }
}

/// What the manager remembers about one rule between ticks. The cooldown spans incidents,
/// so the last notification outlives the breach.
#[derive(Debug, Default)]
struct RuleState {
    breach: Breach,
    last_notified: Option<u64>,
}

/// What one rule's tick reports: its active alert, and whether that alert notifies now.
enum Report {
    Quiet(ActiveAlert),
    Notify(ActiveAlert),
}

impl RuleState {
    /// Advances the rule by one tick and reports its active alert, if an incident is active.
    fn tick(
        &mut self,
        rule: &AlertRule,
        readings: &Readings<'_>,
        now: u64,
        incidents: &mut IncidentSequence,
    ) -> Option<Report> {
        let value = readings.value_for(rule);
        let threshold = readings.threshold_for(rule);
        let breached = is_breached(&rule.operator, value, threshold);
        // Like the breach start, a last notification later than `now` moves back to `now`.
        self.last_notified = self.last_notified.map(|last| last.min(now));
        self.breach =
            std::mem::take(&mut self.breach).advance(breached, rule.duration_secs, now, incidents);

        let (since, incident) = self.breach.active_incident()?;
        let alert = ActiveAlert {
            id: incident.id.to_string(),
            rule: rule.clone(),
            current_value: value,
            fired_at: unix_to_iso8601(incident.activated_at),
            message: alert_message(rule, value, threshold, now.saturating_sub(since)),
        };
        Some(if self.take_notification(rule.cooldown_secs, now) {
            Report::Notify(alert)
        } else {
            Report::Quiet(alert)
        })
    }

    /// Whether the cooldown since the last notification has elapsed; if so, this tick
    /// becomes the last notification.
    fn take_notification(&mut self, cooldown_secs: u64, now: u64) -> bool {
        let due = match self.last_notified {
            None => true,
            Some(last) => now.saturating_sub(last) >= cooldown_secs,
        };
        if due {
            self.last_notified = Some(now);
        }
        due
    }
}

// ── Manager ────────────────────────────────────────────

/// The metric readings of one tick, as `evaluate` receives them.
struct Readings<'a> {
    cpu_percent: f32,
    mem_percent: f32,
    swap_percent: f32,
    disk_usages: &'a HashMap<String, f32>,
    load1: f32,
    load5: f32,
    load15: f32,
    cpu_cores: usize,
}

impl Readings<'_> {
    fn value_for(&self, rule: &AlertRule) -> f32 {
        match &rule.metric {
            AlertMetric::Cpu => self.cpu_percent,
            AlertMetric::Memory => self.mem_percent,
            AlertMetric::Swap => self.swap_percent,
            AlertMetric::Disk => match &rule.mount_point {
                Some(mount_point) => self.disk_usages.get(mount_point).copied().unwrap_or(0.0),
                None => self.disk_usages.values().copied().fold(0.0, f32::max),
            },
            AlertMetric::Load1 => self.load1,
            AlertMetric::Load5 => self.load5,
            AlertMetric::Load15 => self.load15,
        }
    }

    /// A load rule's threshold of 0 means "one per logical core".
    fn threshold_for(&self, rule: &AlertRule) -> f32 {
        let auto = match rule.metric {
            AlertMetric::Load1 | AlertMetric::Load5 | AlertMetric::Load15 => rule.threshold == 0.0,
            AlertMetric::Cpu | AlertMetric::Memory | AlertMetric::Swap | AlertMetric::Disk => false,
        };
        if auto {
            self.cpu_cores as f32
        } else {
            rule.threshold
        }
    }
}

#[derive(Debug)]
pub struct AlertManager {
    rules: Vec<AlertRule>,
    active_alerts: Vec<ActiveAlert>,
    states: HashMap<usize, RuleState>,
    incidents: IncidentSequence,
}

impl AlertManager {
    pub fn new(run: AgentRun, rules: Vec<AlertRule>) -> Self {
        Self {
            rules,
            active_alerts: Vec::new(),
            states: HashMap::new(),
            incidents: IncidentSequence { run, minted: 0 },
        }
    }

    pub fn rules(&self) -> &[AlertRule] {
        &self.rules
    }

    pub fn active_alerts(&self) -> &[ActiveAlert] {
        &self.active_alerts
    }

    /// Replaces the rule set. Per-rule state is keyed by position, so every incident ends
    /// here; otherwise a new rule would inherit the incident of the old rule at its index.
    pub fn replace_rules(&mut self, rules: Vec<AlertRule>) {
        self.rules = rules;
        self.active_alerts.clear();
        self.states.clear();
    }

    pub fn with_defaults(run: AgentRun) -> Self {
        Self::new(
            run,
            vec![
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
            ],
        )
    }

    /// Evaluate all rules against current data. Returns the alerts that notify on this tick.
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
        let readings = Readings {
            cpu_percent,
            mem_percent,
            swap_percent,
            disk_usages,
            load1,
            load5,
            load15,
            cpu_cores,
        };
        let mut notifications = Vec::new();
        let mut active = Vec::new();

        for (index, rule) in self
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.enabled)
        {
            let state = self.states.entry(index).or_default();
            match state.tick(rule, &readings, now_secs, &mut self.incidents) {
                None => {}
                Some(Report::Quiet(alert)) => active.push(alert),
                Some(Report::Notify(alert)) => {
                    notifications.push(alert.clone());
                    active.push(alert);
                }
            }
        }

        self.active_alerts = active;
        notifications
    }
}

fn is_breached(operator: &AlertOperator, value: f32, threshold: f32) -> bool {
    match operator {
        AlertOperator::Gt => value > threshold,
        AlertOperator::Gte => value >= threshold,
        AlertOperator::Lt => value < threshold,
        AlertOperator::Lte => value <= threshold,
    }
}

fn alert_message(rule: &AlertRule, value: f32, threshold: f32, breach_secs: u64) -> String {
    let mount_label = rule
        .mount_point
        .as_ref()
        .map(|m| format!(" on {m}"))
        .unwrap_or_default();
    format!(
        "{} {} {} {:.1}% (threshold: {:.1}%){} for {}s",
        severity_name(&rule.severity),
        metric_name(&rule.metric),
        operator_symbol(&rule.operator),
        value,
        threshold,
        mount_label,
        breach_secs,
    )
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

    /// Evaluation ticks as (cpu percent, now).
    type Ticks = &'static [(f32, u64)];

    fn disk_map(pairs: &[(&str, f32)]) -> HashMap<String, f32> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn cpu_rule(threshold: f32, duration_secs: u64, cooldown_secs: u64) -> AlertRule {
        AlertRule {
            metric: AlertMetric::Cpu,
            operator: AlertOperator::Gt,
            threshold,
            severity: AlertSeverity::Warning,
            duration_secs,
            cooldown_secs,
            mount_point: None,
            enabled: true,
        }
    }

    /// One evaluation tick with only the CPU reading set.
    fn tick_cpu(mgr: &mut AlertManager, cpu_percent: f32, now_secs: u64) -> Vec<ActiveAlert> {
        mgr.evaluate(
            cpu_percent,
            0.0,
            0.0,
            &HashMap::new(),
            0.0,
            0.0,
            0.0,
            4,
            now_secs,
        )
    }

    fn test_run(n: u128) -> AgentRun {
        AgentRun::new(Uuid::from_u128(n))
    }

    fn manager(rules: Vec<AlertRule>) -> AlertManager {
        AlertManager::new(test_run(1), rules)
    }

    /// The incident id of the only active alert, or `None` when no alert is active.
    fn active_id(mgr: &AlertManager) -> Option<String> {
        match mgr.active_alerts.as_slice() {
            [] => None,
            [alert] => Some(alert.id.clone()),
            more => panic!("expected at most one active alert, got {}", more.len()),
        }
    }

    #[test]
    fn an_incident_keeps_one_incident_id_from_activation_through_renotification() {
        // (name, cpu, now, expected notifications); duration 60 s, cooldown 300 s.
        let cases = [
            ("incident activates and notifies", 95.0, 1060, 1),
            ("inside the cooldown", 95.0, 1062, 0),
            ("late inside the cooldown", 95.0, 1359, 0),
            ("renotification after the cooldown", 95.0, 1360, 1),
            ("after the renotification", 95.0, 1362, 0),
        ];
        let mut mgr = manager(vec![cpu_rule(90.0, 60, 300)]);
        tick_cpu(&mut mgr, 95.0, 1000);
        assert_eq!(
            active_id(&mgr),
            None,
            "breach is pending before its duration"
        );

        let mut incident_id = None;
        for (name, cpu, now, notified) in cases {
            let new_alerts = tick_cpu(&mut mgr, cpu, now);
            let id = active_id(&mgr).unwrap_or_else(|| panic!("{name}: no active alert"));
            let first = incident_id.get_or_insert_with(|| id.clone());
            assert_eq!(&id, first, "{name}: active alert keeps the incident id");
            assert_eq!(new_alerts.len(), notified, "{name}: notifications");
            for alert in &new_alerts {
                assert_eq!(
                    &alert.id, first,
                    "{name}: notification carries the incident id"
                );
            }
        }
    }

    /// Breaches from `breach_start`, then returns the incident id seen on each of `ticks`.
    fn incident_ids(mgr: &mut AlertManager, breach_start: u64, ticks: &[u64]) -> Vec<String> {
        tick_cpu(mgr, 95.0, breach_start);
        ticks
            .iter()
            .map(|&now| {
                tick_cpu(mgr, 95.0, now);
                active_id(mgr).unwrap_or_else(|| panic!("no active alert at {now}"))
            })
            .collect()
    }

    #[test]
    fn each_incident_of_a_rule_gets_its_own_incident_id() {
        // (name, rule duration, active ticks of the first incident, active ticks of the
        // second). The first breach starts at 1000, a clear tick at 1009 ends it, and the
        // second breach starts at 1011.
        let cases: [(&str, u64, &[u64], &[u64]); 2] = [
            ("immediate rule", 0, &[1000, 1002], &[1011, 1013]),
            ("rule with a duration", 5, &[1005, 1007], &[1016, 1018]),
        ];
        for (name, duration_secs, first, second) in cases {
            let mut mgr = manager(vec![cpu_rule(90.0, duration_secs, 300)]);
            let first_ids = incident_ids(&mut mgr, 1000, first);
            tick_cpu(&mut mgr, 10.0, 1009);
            let second_ids = incident_ids(&mut mgr, 1011, second);
            for id in &second_ids {
                assert!(
                    !first_ids.contains(id),
                    "{name}: second incident reused {id} from the first ({first_ids:?})"
                );
            }
        }
    }

    /// Runs `ticks` as (cpu, now) and returns the notifications of the last one.
    fn run_ticks(mgr: &mut AlertManager, ticks: Ticks) -> Vec<ActiveAlert> {
        ticks
            .iter()
            .map(|&(cpu, now)| tick_cpu(mgr, cpu, now))
            .last()
            .unwrap_or_default()
    }

    #[test]
    fn fired_at_is_the_tick_the_incident_became_active() {
        // (name, rule duration, ticks as (cpu, now) — the last one has an active alert,
        // expected fired_at)
        let cases: [(&str, u64, Ticks, &str); 6] = [
            (
                "activating exactly at the duration",
                60,
                &[(95.0, 1000), (95.0, 1060)],
                "1970-01-01T00:17:40Z",
            ),
            (
                "activating on the first tick past the duration",
                60,
                &[(95.0, 1000), (95.0, 1030), (95.0, 1061)],
                "1970-01-01T00:17:41Z",
            ),
            (
                "inside the cooldown",
                60,
                &[(95.0, 1000), (95.0, 1061), (95.0, 1063)],
                "1970-01-01T00:17:41Z",
            ),
            (
                "renotification after the cooldown",
                60,
                &[(95.0, 1000), (95.0, 1061), (95.0, 1361)],
                "1970-01-01T00:17:41Z",
            ),
            (
                "a second incident activating inside the previous cooldown",
                0,
                &[(95.0, 1000), (10.0, 1002), (95.0, 1004), (95.0, 1006)],
                "1970-01-01T00:16:44Z",
            ),
            (
                "that incident's first notification, when the cooldown ends",
                0,
                &[
                    (95.0, 1000),
                    (10.0, 1002),
                    (95.0, 1004),
                    (95.0, 1006),
                    (95.0, 1300),
                ],
                "1970-01-01T00:16:44Z",
            ),
        ];
        for (name, duration_secs, ticks, fired_at) in cases {
            let mut mgr = manager(vec![cpu_rule(90.0, duration_secs, 300)]);
            run_ticks(&mut mgr, ticks);
            assert_eq!(mgr.active_alerts.len(), 1, "{name}: active alerts");
            assert_eq!(mgr.active_alerts[0].fired_at, fired_at, "{name}: fired_at");
        }
    }

    #[test]
    fn incident_id_reaches_the_wire_as_a_run_and_sequence_string() {
        // (name, agent run, the run as written on the wire, rule duration, ticks as
        // (cpu, now)). Every case sees two incidents, over three ticks with an active alert.
        let two_incidents: Ticks = &[(95.0, 1000), (95.0, 1002), (10.0, 1004), (95.0, 1006)];
        let aborted_then_two: Ticks = &[
            (95.0, 1000),
            (10.0, 1002),
            (95.0, 1010),
            (95.0, 1015),
            (95.0, 1017),
            (10.0, 1019),
            (95.0, 1021),
            (95.0, 1026),
        ];
        let cases: [(&str, AgentRun, &str, u64, Ticks); 4] = [
            (
                "run 1",
                test_run(1),
                "00000000-0000-0000-0000-000000000001",
                0,
                two_incidents,
            ),
            (
                "run 2 sees the same ticks",
                test_run(2),
                "00000000-0000-0000-0000-000000000002",
                0,
                two_incidents,
            ),
            (
                "a run with hex letters is written in lower case",
                test_run(0xabcdef01_2345_6789_abcd_ef0123456789),
                "abcdef01-2345-6789-abcd-ef0123456789",
                0,
                two_incidents,
            ),
            (
                "a breach that ends before its duration is not an incident",
                test_run(1),
                "00000000-0000-0000-0000-000000000001",
                5,
                aborted_then_two,
            ),
        ];
        for (name, run, run_str, duration_secs, ticks) in cases {
            let mut mgr = AlertManager::new(run, vec![cpu_rule(90.0, duration_secs, 300)]);
            let mut wire_ids = Vec::new();
            for &(cpu, now) in ticks {
                tick_cpu(&mut mgr, cpu, now);
                if let [alert] = mgr.active_alerts.as_slice() {
                    let json = serde_json::to_value(alert).expect("active alert serialises");
                    wire_ids.push(json["id"].clone());
                }
            }
            let expected = ["-1", "-1", "-2"]
                .map(|suffix| serde_json::Value::from(format!("{run_str}{suffix}")));
            assert_eq!(wire_ids, expected, "{name}: wire ids per active tick");
        }
    }

    #[test]
    fn a_first_incident_notifies_even_when_the_clock_reads_less_than_the_cooldown() {
        // (name, ticks as (cpu, now), expected notifications on the last tick) for a rule with
        // a 300 s cooldown.
        let cases: [(&str, Ticks, usize); 4] = [
            ("clock at the epoch", &[(95.0, 0)], 1),
            ("clock just under the cooldown", &[(95.0, 299)], 1),
            ("clock exactly at the cooldown", &[(95.0, 300)], 1),
            (
                "a notification at the epoch starts the cooldown",
                &[(95.0, 0), (95.0, 1)],
                0,
            ),
        ];
        for (name, ticks, notified) in cases {
            let mut mgr = manager(vec![cpu_rule(90.0, 0, 300)]);
            let new_alerts = run_ticks(&mut mgr, ticks);
            assert_eq!(new_alerts.len(), notified, "{name}: notifications");
        }
    }

    #[test]
    fn a_clock_stepped_backwards_restarts_the_wait_from_the_new_reading() {
        // (name, rule duration, ticks as (cpu, now), expected active alerts and notifications
        // on the last tick). Cooldown is 300 s. Each case steps the clock back once; after the
        // step, a duration or cooldown counts from the earlier reading instead of stalling
        // until the clock catches up.
        let cases: [(&str, u64, Ticks, usize, usize); 8] = [
            ("pending breach", 60, &[(95.0, 10_000), (95.0, 6_400)], 0, 0),
            (
                "pending breach one second before a duration after the step",
                60,
                &[(95.0, 10_000), (95.0, 6_400), (95.0, 6_459)],
                0,
                0,
            ),
            (
                // The last notification re-anchors on every tick, breaching or not.
                "a step back on a clear tick re-anchors the cooldown",
                0,
                &[
                    (95.0, 1000),
                    (10.0, 1002),
                    (10.0, 900),
                    (95.0, 950),
                    (95.0, 1200),
                ],
                1,
                1,
            ),
            (
                "pending breach activates a duration after the step",
                60,
                &[(95.0, 10_000), (95.0, 6_400), (95.0, 6_460)],
                1,
                1,
            ),
            (
                "a backwards tick that clears the breach",
                0,
                &[(95.0, 1000), (10.0, 900)],
                0,
                0,
            ),
            (
                "active incident inside its cooldown",
                0,
                &[(95.0, 1000), (95.0, 900)],
                1,
                0,
            ),
            (
                "the cooldown ends a cooldown after the step",
                0,
                &[(95.0, 1000), (95.0, 900), (95.0, 1200)],
                1,
                1,
            ),
            (
                "the cooldown does not end before that",
                0,
                &[(95.0, 1000), (95.0, 900), (95.0, 1199)],
                1,
                0,
            ),
        ];
        for (name, duration_secs, ticks, active, notified) in cases {
            let mut mgr = manager(vec![cpu_rule(90.0, duration_secs, 300)]);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_ticks(&mut mgr, ticks)
            }));
            assert!(outcome.is_ok(), "{name}: evaluate panicked");
            let new_alerts = outcome.unwrap_or_default();
            assert_eq!(mgr.active_alerts.len(), active, "{name}: active alerts");
            assert_eq!(new_alerts.len(), notified, "{name}: notifications");
            for alert in &mgr.active_alerts {
                assert!(
                    !alert.message.contains("18446744"),
                    "{name}: wrapped duration in {:?}",
                    alert.message
                );
            }
        }
    }

    #[test]
    fn a_clock_stepped_backwards_does_not_end_an_active_incident() {
        // (name, ticks after the incident activates at 1060, as (cpu, now)). Breach starts at
        // 1000, duration 60 s; every tick listed still breaches.
        let cases: [(&str, Ticks); 2] = [
            ("a small step back", &[(95.0, 1062), (95.0, 1030)]),
            (
                "a step back before the breach started",
                &[(95.0, 1062), (95.0, 400), (95.0, 402)],
            ),
        ];
        for (name, ticks) in cases {
            let mut mgr = manager(vec![cpu_rule(90.0, 60, 300)]);
            run_ticks(&mut mgr, &[(95.0, 1000), (95.0, 1060)]);
            let before = active_id(&mgr).expect("incident active at 1060");
            for &(cpu, now) in ticks {
                tick_cpu(&mut mgr, cpu, now);
                assert_eq!(
                    active_id(&mgr).as_ref(),
                    Some(&before),
                    "{name}: at {now}, incident id"
                );
                assert_eq!(
                    mgr.active_alerts[0].fired_at, "1970-01-01T00:17:40Z",
                    "{name}: at {now}, fired_at"
                );
            }
        }
    }

    /// One tick where only `metric` reads `value` (a disk reading is the `/` mount), on a
    /// host with 4 logical cores.
    fn tick_metric(mgr: &mut AlertManager, metric: &AlertMetric, value: f32, now: u64) {
        let slot = |m: AlertMetric| if *metric == m { value } else { 0.0 };
        let disks = match metric {
            AlertMetric::Disk => disk_map(&[("/", value)]),
            AlertMetric::Cpu
            | AlertMetric::Memory
            | AlertMetric::Swap
            | AlertMetric::Load1
            | AlertMetric::Load5
            | AlertMetric::Load15 => HashMap::new(),
        };
        mgr.evaluate(
            slot(AlertMetric::Cpu),
            slot(AlertMetric::Memory),
            slot(AlertMetric::Swap),
            &disks,
            slot(AlertMetric::Load1),
            slot(AlertMetric::Load5),
            slot(AlertMetric::Load15),
            4,
            now,
        );
    }

    #[test]
    fn a_zero_threshold_means_one_per_core_only_for_load_metrics() {
        use AlertMetric::*;
        // (name, metric, threshold, reading, breaches) on a 4-core host.
        let cases = [
            ("cpu threshold 0 is a real zero", Cpu, 0.0, 1.0, true),
            ("memory threshold 0 is a real zero", Memory, 0.0, 1.0, true),
            ("swap threshold 0 is a real zero", Swap, 0.0, 1.0, true),
            ("disk threshold 0 is a real zero", Disk, 0.0, 1.0, true),
            ("load1 under the core count", Load1, 0.0, 3.0, false),
            ("load1 over the core count", Load1, 0.0, 5.0, true),
            ("load5 under the core count", Load5, 0.0, 3.0, false),
            ("load5 over the core count", Load5, 0.0, 5.0, true),
            ("load15 under the core count", Load15, 0.0, 3.0, false),
            ("load15 over the core count", Load15, 0.0, 5.0, true),
            (
                "load1 negative threshold is not auto",
                Load1,
                -1.0,
                0.5,
                true,
            ),
            (
                "load5 negative threshold is not auto",
                Load5,
                -1.0,
                0.5,
                true,
            ),
            (
                "load15 negative threshold is not auto",
                Load15,
                -1.0,
                0.5,
                true,
            ),
        ];
        for (name, metric, threshold, reading, breaches) in cases {
            let rule = AlertRule {
                metric: metric.clone(),
                ..cpu_rule(threshold, 0, 0)
            };
            let mut mgr = manager(vec![rule]);
            tick_metric(&mut mgr, &metric, reading, 1000);
            assert_eq!(mgr.active_alerts.len(), usize::from(breaches), "{name}");
        }
    }

    #[test]
    fn alert_message_states_the_effective_threshold_and_the_breach_duration() {
        let load1_auto = AlertRule {
            metric: AlertMetric::Load1,
            ..cpu_rule(0.0, 0, 300)
        };
        // (name, rule, ticks as (reading, now), text the last tick's message contains)
        let cases: [(&str, AlertRule, Ticks, &str); 4] = [
            (
                "auto threshold",
                load1_auto,
                &[(5.0, 1000)],
                "(threshold: 4.0%)",
            ),
            (
                "an incident active for 62 s",
                cpu_rule(90.0, 60, 300),
                &[(95.0, 1000), (95.0, 1060), (95.0, 1062)],
                "for 62s",
            ),
            (
                "right after a clock step back before the breach start",
                cpu_rule(90.0, 60, 300),
                &[(95.0, 1000), (95.0, 1060), (95.0, 900)],
                "for 0s",
            ),
            (
                "counting again after the step",
                cpu_rule(90.0, 60, 300),
                &[(95.0, 1000), (95.0, 1060), (95.0, 900), (95.0, 910)],
                "for 10s",
            ),
        ];
        for (name, rule, ticks, expected) in cases {
            let metric = rule.metric.clone();
            let mut mgr = manager(vec![rule]);
            for &(reading, now) in ticks {
                tick_metric(&mut mgr, &metric, reading, now);
            }
            assert_eq!(mgr.active_alerts.len(), 1, "{name}: active alerts");
            let message = &mgr.active_alerts[0].message;
            assert!(
                message.contains(expected),
                "{name}: {message:?} lacks {expected:?}"
            );
        }
    }

    #[test]
    fn replacing_the_rule_set_ends_every_incident() {
        let mut mgr = manager(vec![cpu_rule(90.0, 0, 300)]);
        tick_cpu(&mut mgr, 95.0, 1000);
        let before = active_id(&mgr).expect("incident active before the replacement");

        mgr.replace_rules(vec![cpu_rule(90.0, 0, 300)]);
        assert_eq!(
            active_id(&mgr),
            None,
            "no incident survives the replacement"
        );

        let new_alerts = tick_cpu(&mut mgr, 95.0, 1002);
        let after = active_id(&mgr).expect("the new rule set's incident is active");
        assert_ne!(
            after, before,
            "the next incident gets an id no earlier one had"
        );
        assert_eq!(
            new_alerts.len(),
            1,
            "the new rule set has no cooldown to inherit"
        );
    }

    #[test]
    fn an_incident_activated_inside_the_previous_cooldown_is_active_but_not_notified() {
        // (name, cpu, now, expected active alerts, expected notifications), per rule duration.
        let immediate = [
            ("first incident activates and notifies", 95.0, 1000, 1, 1),
            ("first incident ends", 10.0, 1002, 0, 0),
            (
                "second incident activates inside the cooldown",
                95.0,
                1004,
                1,
                0,
            ),
            ("still inside the cooldown", 95.0, 1299, 1, 0),
            (
                "cooldown since the first notification elapses",
                95.0,
                1300,
                1,
                1,
            ),
        ];
        let after_duration = [
            ("first breach is pending", 95.0, 1000, 0, 0),
            ("first incident activates and notifies", 95.0, 1005, 1, 1),
            ("first incident ends", 10.0, 1007, 0, 0),
            ("second breach is pending", 95.0, 1009, 0, 0),
            (
                "second incident activates inside the cooldown",
                95.0,
                1014,
                1,
                0,
            ),
            ("still inside the cooldown", 95.0, 1304, 1, 0),
            (
                "cooldown since the first notification elapses",
                95.0,
                1305,
                1,
                1,
            ),
        ];
        for (duration_secs, cases) in [(0, &immediate[..]), (5, &after_duration[..])] {
            let mut mgr = manager(vec![cpu_rule(90.0, duration_secs, 300)]);
            for &(name, cpu, now, active, notified) in cases {
                let new_alerts = tick_cpu(&mut mgr, cpu, now);
                let name = format!("duration {duration_secs}s, {name}");
                assert_eq!(mgr.active_alerts.len(), active, "{name}: active alerts");
                assert_eq!(new_alerts.len(), notified, "{name}: notifications");
            }
        }
    }

    #[test]
    fn incidents_of_different_rules_active_on_the_same_tick_have_distinct_ids() {
        // (name, now): the activating tick, then a tick inside both cooldowns. The two rules
        // are identical, so only their position in the rule set can tell them apart.
        let cases = [
            ("activating tick", 1000),
            ("tick inside the cooldown", 1002),
        ];
        let mut mgr = manager(vec![cpu_rule(90.0, 0, 300), cpu_rule(90.0, 0, 300)]);
        for (name, now) in cases {
            tick_cpu(&mut mgr, 95.0, now);
            assert_eq!(mgr.active_alerts.len(), 2, "{name}: both rules active");
            assert_ne!(
                mgr.active_alerts[0].id, mgr.active_alerts[1].id,
                "{name}: ids must differ across rules"
            );
        }
    }

    #[test]
    fn with_defaults_has_seven_rules_and_load5_disabled() {
        let mgr = AlertManager::with_defaults(test_run(1));
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
        let mut mgr = manager(vec![]);
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
        let mut mgr = manager(vec![rule]);
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
        let mut mgr = manager(vec![rule]);
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
        let mut mgr = manager(vec![rule]);
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
        let mut mgr = manager(vec![rule]);
        let first = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1000);
        assert_eq!(first.len(), 1);

        // Still breached 100s later, within cooldown: no new alert, but still "active" (ongoing).
        let second = mgr.evaluate(95.0, 0.0, 0.0, &HashMap::new(), 0.0, 0.0, 0.0, 4, 1100);
        assert!(second.is_empty());
        assert_eq!(mgr.active_alerts.len(), 1);
        assert_eq!(
            mgr.active_alerts[0].id, first[0].id,
            "the incident keeps its id"
        );

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
        let mut mgr = manager(vec![rule]);
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
            let mut mgr = manager(vec![rule]);
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
        let mut mgr = manager(vec![rule]);
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
        let mut mgr = manager(vec![rule]);
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
        let mut mgr = manager(vec![rule]);
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
        let mut mgr = manager(vec![rule]);
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
        let mut mgr = manager(vec![rule]);
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
        let mut mgr = manager(vec![rule]);
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
