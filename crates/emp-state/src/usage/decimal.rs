//! Small decimal arithmetic surface matching Python Decimal's pricing context.
//! Prices remain decimal strings; binary floating point never computes cost.
use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};
use std::str::FromStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Decimal {
    coefficient: BigInt,
    exponent: i64,
    negative_zero: bool,
}
impl Decimal {
    pub(super) fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let (digits, exponent) = raw
            .split_once(['e', 'E'])
            .map_or(Some((raw, 0)), |(digits, exponent)| {
                Some((digits, exponent.parse::<i64>().ok()?))
            })?;
        let negative = digits.starts_with('-');
        let digits = digits.strip_prefix(['-', '+']).unwrap_or(digits);
        let (integer, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        if integer.is_empty() && fraction.is_empty()
            || !integer
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let coefficient = BigInt::from_str(&format!("{integer}{fraction}")).ok()?;
        if negative && !coefficient.is_zero() {
            return None;
        }
        Some(Self {
            coefficient,
            exponent: exponent.checked_sub(fraction.len() as i64)?,
            negative_zero: negative,
        })
    }
    fn power(exponent: i64) -> Option<BigInt> {
        (0..=10000)
            .contains(&exponent)
            .then(|| BigInt::from(10).pow(exponent as u32))
    }
    fn rounded(coefficient: &BigInt, places: i64, half_even: bool) -> Option<BigInt> {
        let divisor = Self::power(places)?;
        let quotient = coefficient / &divisor;
        let remainder = coefficient % &divisor;
        let doubled = &remainder * 2;
        Some(
            if doubled > divisor
                || doubled == divisor && (!half_even || &quotient % 2 != BigInt::zero())
            {
                quotient + 1
            } else {
                quotient
            },
        )
    }
    fn context(mut self) -> Option<Self> {
        let remove = self.coefficient.to_str_radix(10).len().saturating_sub(28) as i64;
        if remove > 0 {
            self.coefficient = Self::rounded(&self.coefficient, remove, true)?;
            self.exponent = self.exponent.checked_add(remove)?;
        }
        Some(self)
    }
    pub(super) fn multiply(&self, count: u64) -> Option<Self> {
        Self {
            coefficient: &self.coefficient * count,
            exponent: self.exponent,
            negative_zero: self.negative_zero,
        }
        .context()
    }
    pub(super) fn add(&self, other: &Self) -> Option<Self> {
        let exponent = self.exponent.min(other.exponent);
        Self {
            coefficient: &self.coefficient * Self::power(self.exponent - exponent)?
                + &other.coefficient * Self::power(other.exponent - exponent)?,
            exponent,
            negative_zero: false,
        }
        .context()
    }
    pub(super) fn nanos(&self) -> Option<i64> {
        let value = self.multiply(1_000_000_000)?;
        let rounded = if value.exponent < 0 {
            Self::rounded(&value.coefficient, -value.exponent, false)?
        } else {
            value.coefficient * Self::power(value.exponent)?
        };
        rounded.to_i64()
    }
    pub(super) fn zero() -> Self {
        Self {
            coefficient: BigInt::zero(),
            exponent: 0,
            negative_zero: false,
        }
    }
    pub(super) fn numeric_equal(&self, other: &Self) -> bool {
        let exponent = self.exponent.min(other.exponent);
        match (
            Self::power(self.exponent - exponent),
            Self::power(other.exponent - exponent),
        ) {
            (Some(a), Some(b)) => &self.coefficient * a == &other.coefficient * b,
            _ => self == other,
        }
    }
    pub(super) fn canonical(&self) -> String {
        let digits = self.coefficient.to_str_radix(10);
        let adjusted = self.exponent.saturating_add(digits.len() as i64 - 1);
        let sign = if self.negative_zero { "-" } else { "" };
        if self.exponent <= 0 && adjusted >= -6 {
            let point = digits.len() as i64 + self.exponent;
            if self.exponent == 0 {
                return format!("{sign}{digits}");
            }
            if point > 0 {
                let (a, b) = digits.split_at(point as usize);
                return format!("{sign}{a}.{b}");
            }
            return format!("{sign}0.{}{digits}", "0".repeat((-point) as usize));
        }
        let coefficient = if digits.len() == 1 {
            digits
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!(
            "{sign}{coefficient}E{}{adjusted}",
            if adjusted >= 0 { "+" } else { "" }
        )
    }
}
