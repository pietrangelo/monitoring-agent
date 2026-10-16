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

//! Retention policies (RFC 0010 §5, RFC 0012 §2): a period per tier, bounded; the global
//! policy; per-system overrides, each tier following the global policy or fixed, with at most
//! one pending shortening per tier.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::name::SystemKey;
use crate::tier::Tier;

/// One value per tier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerTier<T> {
    raw: T,
    minute: T,
    hour: T,
}

impl<T> PerTier<T> {
    pub fn from_fn(mut f: impl FnMut(Tier) -> T) -> PerTier<T> {
        PerTier {
            raw: f(Tier::Raw),
            minute: f(Tier::Minute),
            hour: f(Tier::Hour),
        }
    }

    pub fn get(&self, tier: Tier) -> &T {
        match tier {
            Tier::Raw => &self.raw,
            Tier::Minute => &self.minute,
            Tier::Hour => &self.hour,
        }
    }

    fn get_mut(&mut self, tier: Tier) -> &mut T {
        match tier {
            Tier::Raw => &mut self.raw,
            Tier::Minute => &mut self.minute,
            Tier::Hour => &mut self.hour,
        }
    }
}

/// A retention period inside its tier's bounds; unconstructible otherwise. Periods are
/// compared by `period()`, only ever within one tier: there is no ordering across tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TierPeriod {
    tier: Tier,
    period: Duration,
}

/// A period outside its tier's bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfBounds {
    pub tier: Tier,
}

impl fmt::Display for OutOfBounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the {} retention must be between {} and {} seconds",
            self.tier.name(),
            self.tier.min_retention_secs(),
            self.tier.max_retention_secs()
        )
    }
}

impl std::error::Error for OutOfBounds {}

impl TierPeriod {
    pub fn new(tier: Tier, period: Duration) -> Result<TierPeriod, OutOfBounds> {
        let bounds = Duration::from_secs(tier.min_retention_secs())
            ..=Duration::from_secs(tier.max_retention_secs());
        match bounds.contains(&period) {
            true => Ok(TierPeriod { tier, period }),
            false => Err(OutOfBounds { tier }),
        }
    }

    /// The tier's default period: 24 h of raw points, 14 days of minutes, 30 days of hours.
    pub fn default_for(tier: Tier) -> TierPeriod {
        const DAY: u64 = 86_400;
        let days = match tier {
            Tier::Raw => 1,
            Tier::Minute => 14,
            Tier::Hour => 30,
        };
        TierPeriod {
            tier,
            period: Duration::from_secs(days * DAY),
        }
    }

    pub fn tier(self) -> Tier {
        self.tier
    }

    pub fn period(self) -> Duration {
        self.period
    }
}

/// The global policy: one period per tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    periods: PerTier<TierPeriod>,
}

impl Default for RetentionPolicy {
    /// 24 h of raw points, 14 days of minutes, 30 days of hours.
    fn default() -> RetentionPolicy {
        RetentionPolicy {
            periods: PerTier::from_fn(TierPeriod::default_for),
        }
    }
}

impl RetentionPolicy {
    /// The policy with `period` in its own tier.
    pub fn with(mut self, period: TierPeriod) -> RetentionPolicy {
        *self.periods.get_mut(period.tier()) = period;
        self
    }

    pub fn period(&self, tier: Tier) -> TierPeriod {
        *self.periods.get(tier)
    }
}

/// What one tier of an override follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TierSetting {
    /// The global policy, resolved whenever it is read.
    Global,
    Fixed(TierPeriod),
}

impl TierSetting {
    /// The period the setting stands for under `global`.
    pub fn resolve(self, global: TierPeriod) -> TierPeriod {
        match self {
            TierSetting::Global => global,
            TierSetting::Fixed(period) => period,
        }
    }
}

/// A shortening waiting out its delay before it applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingShortening {
    pub next: TierSetting,
    pub delay: Duration,
}

/// One tier of an override: what is enforced now, and at most one pending shortening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierOverride {
    setting: TierSetting,
    pending: Option<PendingShortening>,
}

impl Default for TierOverride {
    fn default() -> TierOverride {
        TierOverride {
            setting: TierSetting::Global,
            pending: None,
        }
    }
}

impl TierOverride {
    /// A tier as persisted: its setting and its pending shortening.
    pub fn new(setting: TierSetting, pending: Option<PendingShortening>) -> TierOverride {
        TierOverride { setting, pending }
    }

    pub fn setting(&self) -> TierSetting {
        self.setting
    }

    pub fn pending(&self) -> Option<PendingShortening> {
        self.pending
    }

    /// The period enforced now under `global`.
    pub fn enforced(&self, global: TierPeriod) -> TierPeriod {
        self.setting.resolve(global)
    }

    /// RFC 0012 §2: a change at least as long as what is enforced applies at once and drops
    /// any pending entry; a shorter one becomes the one pending entry, re-armed with `delay`.
    pub fn change(
        self,
        requested: TierSetting,
        global: TierPeriod,
        delay: Duration,
    ) -> TierOverride {
        match requested.resolve(global).period() >= self.enforced(global).period() {
            true => TierOverride::new(requested, None),
            false => TierOverride::new(
                self.setting,
                Some(PendingShortening {
                    next: requested,
                    delay,
                }),
            ),
        }
    }

    /// The pending shortening applied: its delay has ended.
    pub fn take_effect(self) -> TierOverride {
        match self.pending {
            Some(pending) => TierOverride::new(pending.next, None),
            None => self,
        }
    }

    fn follows_global(&self) -> bool {
        self.setting == TierSetting::Global && self.pending.is_none()
    }

    /// Whether every fixed period it holds, enforced or pending, is of `tier`.
    fn is_of(&self, tier: Tier) -> bool {
        let pending = self.pending.map(|p| p.next);
        [Some(self.setting), pending]
            .into_iter()
            .flatten()
            .all(|setting| match setting {
                TierSetting::Global => true,
                TierSetting::Fixed(period) => period.tier() == tier,
            })
    }
}

/// A system's override: one setting per tier, never a copy of the global policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Override {
    tiers: PerTier<TierOverride>,
}

/// An override holding a fixed period in another tier's slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierMismatch {
    /// The slot holding it.
    pub tier: Tier,
}

impl Override {
    /// An override as persisted: refused unless every fixed period sits in its own tier.
    pub fn new(tiers: PerTier<TierOverride>) -> Result<Override, TierMismatch> {
        match Tier::ALL.into_iter().find(|t| !tiers.get(*t).is_of(*t)) {
            Some(tier) => Err(TierMismatch { tier }),
            None => Ok(Override { tiers }),
        }
    }

    pub fn tier(&self, tier: Tier) -> &TierOverride {
        self.tiers.get(tier)
    }

    /// Every tier follows the global policy with nothing pending: the override is removed.
    pub fn follows_global(&self) -> bool {
        Tier::ALL.iter().all(|t| self.tier(*t).follows_global())
    }
}

/// A requested override: a period for each tier named, the global policy for the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionChange {
    tiers: PerTier<TierSetting>,
}

/// A change naming one tier twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DuplicateTier(pub Tier);

impl RetentionChange {
    /// Each period in its own tier; a tier not named follows the global policy.
    pub fn new(
        periods: impl IntoIterator<Item = TierPeriod>,
    ) -> Result<RetentionChange, DuplicateTier> {
        periods
            .into_iter()
            .try_fold(RetentionChange::follow_global(), |mut change, period| {
                let slot = change.tiers.get_mut(period.tier());
                match slot {
                    TierSetting::Global => {
                        *slot = TierSetting::Fixed(period);
                        Ok(change)
                    }
                    TierSetting::Fixed(_) => Err(DuplicateTier(period.tier())),
                }
            })
    }

    /// Every tier back to the global policy (`DELETE .../retention`).
    pub fn follow_global() -> RetentionChange {
        RetentionChange {
            tiers: PerTier::from_fn(|_| TierSetting::Global),
        }
    }

    pub fn tier(&self, tier: Tier) -> TierSetting {
        *self.tiers.get(tier)
    }
}

/// What a change did to a system's override: `None` is no override (the global policy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionOutcome {
    pub before: Option<Override>,
    pub after: Option<Override>,
}

/// The global policy and every override.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policies {
    global: RetentionPolicy,
    overrides: BTreeMap<SystemKey, Override>,
}

impl Policies {
    pub fn new(global: RetentionPolicy) -> Policies {
        Policies {
            global,
            overrides: BTreeMap::new(),
        }
    }

    pub fn global(&self) -> &RetentionPolicy {
        &self.global
    }

    pub fn override_of(&self, system: &SystemKey) -> Option<&Override> {
        self.overrides.get(system)
    }

    /// Puts back an override as persisted (at open); one that follows the global policy is
    /// not kept.
    pub fn restore(&mut self, system: SystemKey, over: Override) {
        self.put(system, over);
    }

    /// The period enforced for a system's tier: its override's, else the global policy's.
    pub fn effective(&self, system: &SystemKey, tier: Tier) -> TierPeriod {
        let global = self.global.period(tier);
        self.overrides
            .get(system)
            .map_or(global, |over| over.tier(tier).enforced(global))
    }

    /// The longest period enforced for any system in the tier: no chunk of the tier older
    /// than this is live.
    pub fn longest(&self, tier: Tier) -> TierPeriod {
        let global = self.global.period(tier);
        self.overrides
            .values()
            .map(|over| over.tier(tier).enforced(global))
            .fold(global, |longest, p| match p.period() > longest.period() {
                true => p,
                false => longest,
            })
    }

    /// Applies a requested override tier by tier (RFC 0012 §2).
    pub fn change(
        &mut self,
        system: &SystemKey,
        change: &RetentionChange,
        delay: Duration,
    ) -> RetentionOutcome {
        // Each tier's change keeps or takes periods of its own tier: no mismatch can arise.
        self.update(system, |over, global| Override {
            tiers: PerTier::from_fn(|tier| {
                over.tier(tier)
                    .change(change.tier(tier), global.period(tier), delay)
            }),
        })
    }

    /// Applies the tier's pending shortening, if any: its delay has ended.
    pub fn take_effect(&mut self, system: &SystemKey, tier: Tier) -> RetentionOutcome {
        match self.overrides.contains_key(system) {
            false => RetentionOutcome {
                before: None,
                after: None,
            },
            true => self.update(system, |mut over, _| {
                *over.tiers.get_mut(tier) = over.tier(tier).take_effect();
                over
            }),
        }
    }

    /// Replaces a system's override with `f` of it (the default when it has none).
    fn update(
        &mut self,
        system: &SystemKey,
        f: impl FnOnce(Override, &RetentionPolicy) -> Override,
    ) -> RetentionOutcome {
        let before = self.overrides.get(system).copied();
        let after = f(before.unwrap_or_default(), &self.global);
        self.put(system.clone(), after);
        RetentionOutcome {
            before,
            after: self.overrides.get(system).copied(),
        }
    }

    /// Keeps an override, or drops it when it follows the global policy.
    fn put(&mut self, system: SystemKey, over: Override) {
        match over.follows_global() {
            true => self.overrides.remove(&system),
            false => self.overrides.insert(system, over),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;
    const DELAY: Duration = Duration::from_secs(600);

    fn period(tier: Tier, secs: u64) -> TierPeriod {
        TierPeriod::new(tier, Duration::from_secs(secs)).expect("in bounds")
    }

    fn fixed(tier: Tier, secs: u64) -> TierSetting {
        TierSetting::Fixed(period(tier, secs))
    }

    fn pending(next: TierSetting) -> Option<PendingShortening> {
        Some(PendingShortening { next, delay: DELAY })
    }

    fn system(name: &str) -> SystemKey {
        SystemKey::try_from(name.as_bytes()).expect("key")
    }

    #[test]
    fn a_tier_period_is_refused_outside_its_tiers_bounds() {
        let cases = [
            ("raw below an hour", Tier::Raw, HOUR - 1, false),
            ("raw at an hour", Tier::Raw, HOUR, true),
            ("raw at 30 days", Tier::Raw, 30 * DAY, true),
            ("raw past 30 days", Tier::Raw, 30 * DAY + 1, false),
            ("minute below a day", Tier::Minute, DAY - 1, false),
            ("minute at a day", Tier::Minute, DAY, true),
            ("minute at 400 days", Tier::Minute, 400 * DAY, true),
            ("minute past 400 days", Tier::Minute, 400 * DAY + 1, false),
            ("hour below 7 days", Tier::Hour, 7 * DAY - 1, false),
            ("hour at 7 days", Tier::Hour, 7 * DAY, true),
            ("hour at 10 years", Tier::Hour, 3_650 * DAY, true),
            ("hour past 10 years", Tier::Hour, 3_650 * DAY + 1, false),
            ("zero", Tier::Raw, 0, false),
        ];
        for (name, tier, secs, valid) in cases {
            let made = TierPeriod::new(tier, Duration::from_secs(secs));
            match valid {
                true => {
                    let made = made.expect(name);
                    assert_eq!(made.tier(), tier, "{name}");
                    assert_eq!(made.period(), Duration::from_secs(secs), "{name}");
                }
                false => assert_eq!(made, Err(OutOfBounds { tier }), "{name}"),
            }
        }
    }

    #[test]
    fn the_default_policy_keeps_a_day_of_raw_two_weeks_of_minutes_and_a_month_of_hours() {
        let policy = RetentionPolicy::default();
        let cases = [
            (Tier::Raw, 24 * HOUR),
            (Tier::Minute, 14 * DAY),
            (Tier::Hour, 30 * DAY),
        ];
        for (tier, secs) in cases {
            assert_eq!(policy.period(tier), period(tier, secs), "{tier:?}");
            assert_eq!(
                TierPeriod::default_for(tier),
                period(tier, secs),
                "{tier:?}"
            );
        }
    }

    #[test]
    fn a_policy_takes_each_period_into_its_own_tier_and_keeps_the_others() {
        let policy = RetentionPolicy::default()
            .with(period(Tier::Hour, 90 * DAY))
            .with(period(Tier::Raw, 2 * HOUR));
        assert_eq!(policy.period(Tier::Raw), period(Tier::Raw, 2 * HOUR));
        assert_eq!(policy.period(Tier::Minute), period(Tier::Minute, 14 * DAY));
        assert_eq!(policy.period(Tier::Hour), period(Tier::Hour, 90 * DAY));
    }

    #[test]
    fn a_setting_resolves_global_to_the_global_period_and_fixed_to_its_own() {
        let global = period(Tier::Raw, 24 * HOUR);
        assert_eq!(TierSetting::Global.resolve(global), global);
        assert_eq!(
            fixed(Tier::Raw, 2 * HOUR).resolve(global),
            period(Tier::Raw, 2 * HOUR)
        );
    }

    #[test]
    fn a_tier_change_applies_at_once_unless_it_shortens_what_is_enforced() {
        let global = period(Tier::Raw, 24 * HOUR);
        let at = |setting, pending| TierOverride::new(setting, pending);
        let cases = [
            (
                "longer applies at once",
                at(TierSetting::Global, None),
                fixed(Tier::Raw, 48 * HOUR),
                at(fixed(Tier::Raw, 48 * HOUR), None),
            ),
            (
                "shorter is pending, the enforced setting kept",
                at(TierSetting::Global, None),
                fixed(Tier::Raw, HOUR),
                at(TierSetting::Global, pending(fixed(Tier::Raw, HOUR))),
            ),
            (
                "shorter again replaces the pending entry",
                at(TierSetting::Global, pending(fixed(Tier::Raw, HOUR))),
                fixed(Tier::Raw, 2 * HOUR),
                at(TierSetting::Global, pending(fixed(Tier::Raw, 2 * HOUR))),
            ),
            (
                "longer than enforced drops the pending entry",
                at(TierSetting::Global, pending(fixed(Tier::Raw, HOUR))),
                fixed(Tier::Raw, 48 * HOUR),
                at(fixed(Tier::Raw, 48 * HOUR), None),
            ),
            (
                "exactly the enforced value drops the pending entry",
                at(fixed(Tier::Raw, 48 * HOUR), pending(fixed(Tier::Raw, HOUR))),
                fixed(Tier::Raw, 48 * HOUR),
                at(fixed(Tier::Raw, 48 * HOUR), None),
            ),
            (
                "back to global from a longer override is a shortening",
                at(fixed(Tier::Raw, 48 * HOUR), None),
                TierSetting::Global,
                at(fixed(Tier::Raw, 48 * HOUR), pending(TierSetting::Global)),
            ),
            (
                "back to global from a shorter override applies at once",
                at(fixed(Tier::Raw, HOUR), None),
                TierSetting::Global,
                at(TierSetting::Global, None),
            ),
            (
                "global over a global setting equal to it drops a pending entry",
                at(TierSetting::Global, pending(fixed(Tier::Raw, HOUR))),
                TierSetting::Global,
                at(TierSetting::Global, None),
            ),
            (
                "fixed at the global value replaces global at once",
                at(TierSetting::Global, None),
                fixed(Tier::Raw, 24 * HOUR),
                at(fixed(Tier::Raw, 24 * HOUR), None),
            ),
        ];
        for (name, before, requested, after) in cases {
            assert_eq!(before.change(requested, global, DELAY), after, "{name}");
        }
    }

    #[test]
    fn a_pending_shortening_carries_the_delay_it_was_armed_with() {
        let global = period(Tier::Minute, 14 * DAY);
        let delay = Duration::from_secs(1_234);
        let after = TierOverride::default().change(fixed(Tier::Minute, DAY), global, delay);
        assert_eq!(
            after.pending(),
            Some(PendingShortening {
                next: fixed(Tier::Minute, DAY),
                delay
            })
        );
    }

    #[test]
    fn an_override_holding_a_period_in_another_tiers_slot_is_refused() {
        let hour = fixed(Tier::Hour, 90 * DAY);
        let raw = fixed(Tier::Raw, 2 * HOUR);
        let minute = fixed(Tier::Minute, DAY);
        let enforced = |setting| TierOverride::new(setting, None);
        let waiting = |next| TierOverride::new(TierSetting::Global, pending(next));
        let global = TierOverride::default();
        // (name, the raw, minute and hour slots, expected)
        let cases = [
            ("its own tier", [global, global, enforced(hour)], Ok(())),
            (
                "every tier its own, enforced and pending",
                [
                    enforced(raw),
                    waiting(minute),
                    TierOverride::new(hour, pending(hour)),
                ],
                Ok(()),
            ),
            (
                "global with a pending global",
                [waiting(TierSetting::Global), global, global],
                Ok(()),
            ),
            (
                "enforced in the raw slot",
                [enforced(hour), global, global],
                Err(TierMismatch { tier: Tier::Raw }),
            ),
            (
                "pending in the minute slot",
                [global, waiting(hour), global],
                Err(TierMismatch { tier: Tier::Minute }),
            ),
            (
                "enforced in the hour slot",
                [global, global, enforced(raw)],
                Err(TierMismatch { tier: Tier::Hour }),
            ),
            (
                "an own-tier setting does not excuse a pending mismatch",
                [global, global, TierOverride::new(hour, pending(raw))],
                Err(TierMismatch { tier: Tier::Hour }),
            ),
            (
                "two offenders: the first slot is named",
                [global, enforced(hour), enforced(minute)],
                Err(TierMismatch { tier: Tier::Minute }),
            ),
        ];
        for (name, [r, m, h], expected) in cases {
            let tiers = PerTier::from_fn(|tier| match tier {
                Tier::Raw => r,
                Tier::Minute => m,
                Tier::Hour => h,
            });
            assert_eq!(Override::new(tiers).map(|_| ()), expected, "{name}");
        }
    }

    #[test]
    fn a_replaced_shortening_is_re_armed_with_the_new_delay() {
        let global = period(Tier::Raw, 24 * HOUR);
        let first = TierOverride::default().change(fixed(Tier::Raw, HOUR), global, DELAY);
        let later = Duration::from_secs(900);
        assert_eq!(
            first
                .change(fixed(Tier::Raw, 2 * HOUR), global, later)
                .pending(),
            Some(PendingShortening {
                next: fixed(Tier::Raw, 2 * HOUR),
                delay: later
            })
        );
    }

    #[test]
    fn a_partial_change_compares_every_tier_and_takes_effect_one_tier_at_a_time() {
        let mut policies = Policies::new(RetentionPolicy::default());
        let start = Override::new(PerTier::from_fn(|tier| match tier {
            Tier::Raw => TierOverride::new(fixed(Tier::Raw, 48 * HOUR), None),
            Tier::Minute => TierOverride::default(),
            Tier::Hour => TierOverride::new(fixed(Tier::Hour, 90 * DAY), None),
        }))
        .expect("tiers");
        policies.restore(system("s"), start);
        let delay = Duration::from_secs(1_234);
        let raw_only = RetentionChange::new([period(Tier::Raw, 72 * HOUR)]).expect("one tier");
        let mixed = Override::new(PerTier::from_fn(|tier| match tier {
            Tier::Raw => TierOverride::new(fixed(Tier::Raw, 72 * HOUR), None),
            Tier::Minute => TierOverride::default(),
            Tier::Hour => TierOverride::new(
                fixed(Tier::Hour, 90 * DAY),
                Some(PendingShortening {
                    next: TierSetting::Global,
                    delay,
                }),
            ),
        }))
        .expect("tiers");
        assert_eq!(
            policies.change(&system("s"), &raw_only, delay),
            RetentionOutcome {
                before: Some(start),
                after: Some(mixed)
            },
            "raw lengthened at once; the omitted hour tier is a pending shortening to global"
        );
        assert_eq!(
            policies.take_effect(&system("s"), Tier::Raw),
            RetentionOutcome {
                before: Some(mixed),
                after: Some(mixed)
            },
            "only the named tier's pending entry takes effect"
        );
        let hour_applied = Override::new(PerTier::from_fn(|tier| match tier {
            Tier::Raw => TierOverride::new(fixed(Tier::Raw, 72 * HOUR), None),
            Tier::Minute | Tier::Hour => TierOverride::default(),
        }))
        .expect("tiers");
        assert_eq!(
            policies.take_effect(&system("s"), Tier::Hour).after,
            Some(hour_applied)
        );
        assert_eq!(
            policies.effective(&system("s"), Tier::Hour),
            period(Tier::Hour, 30 * DAY)
        );

        policies.change(&system("s"), &RetentionChange::follow_global(), delay);
        assert_eq!(
            policies.effective(&system("s"), Tier::Raw),
            period(Tier::Raw, 72 * HOUR),
            "dropping a longer override is a shortening"
        );
        assert_eq!(
            policies.take_effect(&system("s"), Tier::Raw),
            RetentionOutcome {
                before: Some(
                    Override::new(PerTier::from_fn(|tier| match tier {
                        Tier::Raw => TierOverride::new(
                            fixed(Tier::Raw, 72 * HOUR),
                            Some(PendingShortening {
                                next: TierSetting::Global,
                                delay,
                            }),
                        ),
                        Tier::Minute | Tier::Hour => TierOverride::default(),
                    }))
                    .expect("tiers")
                ),
                after: None
            },
            "the last pending global applied removes the override"
        );
        assert_eq!(policies.override_of(&system("s")), None);
    }

    #[test]
    fn taking_effect_applies_the_pending_setting_and_clears_it() {
        let cases = [
            (
                "a fixed shortening",
                TierOverride::new(TierSetting::Global, pending(fixed(Tier::Raw, HOUR))),
                TierOverride::new(fixed(Tier::Raw, HOUR), None),
            ),
            (
                "back to global",
                TierOverride::new(fixed(Tier::Raw, 48 * HOUR), pending(TierSetting::Global)),
                TierOverride::new(TierSetting::Global, None),
            ),
            (
                "nothing pending is unchanged",
                TierOverride::new(fixed(Tier::Raw, 2 * HOUR), None),
                TierOverride::new(fixed(Tier::Raw, 2 * HOUR), None),
            ),
        ];
        for (name, before, after) in cases {
            assert_eq!(before.take_effect(), after, "{name}");
        }
    }

    #[test]
    fn a_change_puts_each_period_in_its_tier_and_leaves_the_others_global() {
        let change =
            RetentionChange::new([period(Tier::Hour, 90 * DAY), period(Tier::Raw, 2 * HOUR)])
                .expect("distinct tiers");
        assert_eq!(change.tier(Tier::Raw), fixed(Tier::Raw, 2 * HOUR));
        assert_eq!(change.tier(Tier::Minute), TierSetting::Global);
        assert_eq!(change.tier(Tier::Hour), fixed(Tier::Hour, 90 * DAY));
        assert_eq!(
            RetentionChange::new([period(Tier::Raw, 2 * HOUR), period(Tier::Raw, 3 * HOUR)]),
            Err(DuplicateTier(Tier::Raw))
        );
        assert_eq!(
            RetentionChange::new([
                period(Tier::Hour, 90 * DAY),
                period(Tier::Raw, 2 * HOUR),
                period(Tier::Hour, 60 * DAY)
            ]),
            Err(DuplicateTier(Tier::Hour)),
            "a duplicate apart from its first is caught and named"
        );
        assert_eq!(
            RetentionChange::new([]),
            Ok(RetentionChange::follow_global())
        );
    }

    #[test]
    fn a_system_follows_its_override_and_every_other_system_the_global_policy() {
        let mut policies = Policies::new(RetentionPolicy::default());
        let change = RetentionChange::new([period(Tier::Raw, 48 * HOUR)]).expect("one tier");
        policies.change(&system("long"), &change, DELAY);
        let cases = [
            ("long", Tier::Raw, 48 * HOUR),
            ("long", Tier::Minute, 14 * DAY),
            ("other", Tier::Raw, 24 * HOUR),
        ];
        for (name, tier, secs) in cases {
            assert_eq!(
                policies.effective(&system(name), tier),
                period(tier, secs),
                "{name} {tier:?}"
            );
        }
    }

    #[test]
    fn a_shortening_is_enforced_only_once_it_takes_effect() {
        let mut policies = Policies::new(RetentionPolicy::default());
        let short = RetentionChange::new([period(Tier::Raw, HOUR)]).expect("one tier");
        let outcome = policies.change(&system("s"), &short, DELAY);
        let waiting = Override::new(PerTier::from_fn(|tier| match tier {
            Tier::Raw => TierOverride::new(TierSetting::Global, pending(fixed(Tier::Raw, HOUR))),
            Tier::Minute | Tier::Hour => TierOverride::default(),
        }))
        .expect("tiers");
        assert_eq!(
            outcome,
            RetentionOutcome {
                before: None,
                after: Some(waiting)
            }
        );
        assert_eq!(
            policies.effective(&system("s"), Tier::Raw),
            period(Tier::Raw, 24 * HOUR)
        );

        let applied = Override::new(PerTier::from_fn(|tier| match tier {
            Tier::Raw => TierOverride::new(fixed(Tier::Raw, HOUR), None),
            Tier::Minute | Tier::Hour => TierOverride::default(),
        }))
        .expect("tiers");
        assert_eq!(
            policies.take_effect(&system("s"), Tier::Raw),
            RetentionOutcome {
                before: Some(waiting),
                after: Some(applied)
            }
        );
        assert_eq!(
            policies.effective(&system("s"), Tier::Raw),
            period(Tier::Raw, HOUR)
        );
    }

    #[test]
    fn an_override_that_follows_the_global_policy_is_removed() {
        let mut policies = Policies::new(RetentionPolicy::default());
        let short = RetentionChange::new([period(Tier::Raw, HOUR)]).expect("one tier");
        policies.change(&system("s"), &short, DELAY);
        policies.take_effect(&system("s"), Tier::Raw);
        let outcome = policies.change(&system("s"), &RetentionChange::follow_global(), DELAY);
        assert_eq!(
            outcome.after, None,
            "lengthening back to global applies at once"
        );
        assert_eq!(policies.override_of(&system("s")), None);

        let mut restored = Policies::new(RetentionPolicy::default());
        restored.restore(system("g"), Override::default());
        assert_eq!(
            restored.override_of(&system("g")),
            None,
            "restore drops a no-op"
        );
        let kept = Override::new(PerTier::from_fn(|tier| match tier {
            Tier::Hour => TierOverride::new(fixed(Tier::Hour, 90 * DAY), None),
            Tier::Raw | Tier::Minute => TierOverride::default(),
        }))
        .expect("tiers");
        restored.restore(system("h"), kept);
        assert_eq!(restored.override_of(&system("h")), Some(&kept));
        assert_eq!(
            restored.effective(&system("h"), Tier::Hour),
            period(Tier::Hour, 90 * DAY)
        );
    }

    #[test]
    fn taking_effect_without_a_pending_shortening_changes_nothing() {
        let mut policies = Policies::new(RetentionPolicy::default());
        let none = policies.take_effect(&system("absent"), Tier::Raw);
        assert_eq!(
            none,
            RetentionOutcome {
                before: None,
                after: None
            }
        );
        let long = RetentionChange::new([period(Tier::Raw, 48 * HOUR)]).expect("one tier");
        let set = policies.change(&system("s"), &long, DELAY).after;
        let longer = Override::new(PerTier::from_fn(|tier| match tier {
            Tier::Raw => TierOverride::new(fixed(Tier::Raw, 48 * HOUR), None),
            Tier::Minute | Tier::Hour => TierOverride::default(),
        }))
        .expect("tiers");
        assert_eq!(set, Some(longer), "a lengthening applies at once");
        assert_eq!(
            policies.take_effect(&system("s"), Tier::Raw),
            RetentionOutcome {
                before: set,
                after: set
            }
        );
    }

    #[test]
    fn a_global_setting_follows_a_new_global_policy() {
        let shorter = RetentionPolicy::default().with(period(Tier::Raw, 12 * HOUR));
        let mut policies = Policies::new(shorter);
        let over = Override::new(PerTier::from_fn(|tier| match tier {
            Tier::Minute => TierOverride::new(fixed(Tier::Minute, 30 * DAY), None),
            Tier::Raw | Tier::Hour => TierOverride::default(),
        }))
        .expect("tiers");
        policies.restore(system("s"), over);
        assert_eq!(
            policies.effective(&system("s"), Tier::Raw),
            period(Tier::Raw, 12 * HOUR)
        );
        assert_eq!(
            policies.effective(&system("s"), Tier::Minute),
            period(Tier::Minute, 30 * DAY)
        );
    }

    #[test]
    fn the_longest_period_of_a_tier_is_the_longest_any_system_enforces() {
        let mut policies = Policies::new(RetentionPolicy::default());
        let cases = [
            ("only the global policy", None, Tier::Raw, 24 * HOUR),
            (
                "a longer override",
                Some(("a", period(Tier::Raw, 72 * HOUR))),
                Tier::Raw,
                72 * HOUR,
            ),
            (
                "a shorter override doesn't lower it",
                Some(("b", period(Tier::Raw, 2 * HOUR))),
                Tier::Raw,
                72 * HOUR,
            ),
            ("another tier is untouched", None, Tier::Minute, 14 * DAY),
            (
                "a pending shortening still enforces the longer period",
                Some(("a", period(Tier::Raw, HOUR))),
                Tier::Raw,
                72 * HOUR,
            ),
            (
                "the longest override on a later key",
                Some(("z", period(Tier::Raw, 96 * HOUR))),
                Tier::Raw,
                96 * HOUR,
            ),
        ];
        for (name, set, tier, secs) in cases {
            if let Some((who, p)) = set {
                let change = RetentionChange::new([p]).expect("one tier");
                policies.change(&system(who), &change, DELAY);
            }
            assert_eq!(policies.longest(tier), period(tier, secs), "{name}");
        }
    }
}
