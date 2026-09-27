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

//! Usage values and the rules that derive them.

/// A share of a resource capacity, from 0 to 100 inclusive.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Percent(f32);

impl Percent {
    /// `value` held to 0–100, or `None` for NaN, which has no place on the scale.
    pub fn saturating(value: f32) -> Option<Self> {
        // `+ 0.0` turns -0.0 into 0.0, which `clamp` keeps.
        (!value.is_nan()).then(|| Self(value.clamp(0.0, 100.0) + 0.0))
    }

    pub fn get(self) -> f32 {
        self.0
    }
}

/// A load average: runnable tasks averaged over a window. Not a percent, and not bounded
/// above: 150 is a valid load on a 128-core host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadAverage(f32);

impl LoadAverage {
    pub fn new(value: f32) -> Self {
        Self(value)
    }

    pub fn get(self) -> f32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percent_is_held_to_zero_through_one_hundred() {
        // (name, value, expected)
        let cases = [
            ("zero", 0.0, Some(0.0)),
            ("negative zero becomes zero", -0.0, Some(0.0)),
            (
                "smallest positive",
                f32::MIN_POSITIVE,
                Some(f32::MIN_POSITIVE),
            ),
            ("a tiny share", 1e-7, Some(1e-7)),
            ("just above zero", 0.1, Some(0.1)),
            ("full precision", 12.345_678, Some(12.345_678)),
            ("inside the scale", 33.3, Some(33.3)),
            ("just under one hundred", 99.9, Some(99.9)),
            ("exactly one hundred", 100.0, Some(100.0)),
            ("just over one hundred", 100.3, Some(100.0)),
            ("far over one hundred", 1e9, Some(100.0)),
            ("positive infinity", f32::INFINITY, Some(100.0)),
            ("just under zero", -0.1, Some(0.0)),
            ("negative infinity", f32::NEG_INFINITY, Some(0.0)),
            ("not a number", f32::NAN, None),
        ];
        for (name, value, expected) in cases {
            // Compared bit for bit, so `-0.0` can't pass for `0.0`.
            assert_eq!(
                Percent::saturating(value).map(|p| p.get().to_bits()),
                expected.map(f32::to_bits),
                "{name}"
            );
        }
    }
}
