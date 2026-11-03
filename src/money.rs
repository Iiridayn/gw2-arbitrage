use num_rational::{Rational32, Rational64};
use num_traits::{Signed, ToPrimitive, Zero};

use std::array;
use std::cmp;
use std::convert::TryFrom;
use std::fmt;

use crate::config::CONFIG;

// https://wiki.guildwars2.com/wiki/Trading_Post
// Listing Fee (5%) — This nonrefundable cost covers listing and holding your items for sale. This
// fee has a minimum of 1c and is immediately taken from your wallet when you list or instantly
// sell an item.
// Exchange Fee (10%) — This fee is the Trading Post's cut of the profit. This fee has a minimum of
// 1c and is deducted from coins delivered to the seller after a successful sale.
const TRADING_POST_LISTING_FEE: u8 = 5; // %
const TRADING_POST_EXCHANGE_FEE: u8 = 10; // %

/// Currencies other than coin, valued by their configured rate in copper
// TODO: laurels,
// badges of honor? Testimony/proof of heroics
// Geodes, Bandit Crests, Airship Parts, Aurillium, Ley Crystals, Trade Contracts, Racing Medallions
// Fractal Relics
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Currency {
    Karma,
    UnboundMagic,
    VolatileMagic,
    ResearchNote,
    SpiritShard,
}
use Currency::*;

impl Currency {
    pub const ALL: [Currency; 5] = [Karma, UnboundMagic, VolatileMagic, ResearchNote, SpiritShard];

    /// API currency id
    fn id(self) -> u32 {
        match self {
            Karma => 2,
            UnboundMagic => 32,
            VolatileMagic => 45,
            ResearchNote => 61,
            SpiritShard => 23,
        }
    }

    fn abbrev(self) -> &'static str {
        match self {
            Karma => "Karma",
            UnboundMagic => "UM",
            VolatileMagic => "VM",
            ResearchNote => "RN",
            SpiritShard => "SS",
        }
    }
}

const COIN_ID: u32 = 1;

#[derive(Copy, Clone, Eq)]
pub struct Money {
    copper: Rational32,
    // indexed by `Currency as usize`
    currencies: [Rational32; Currency::ALL.len()],
}
impl Money {
    pub fn from_copper(copper: i32) -> Self {
        Self {
            copper: Rational32::from(copper),
            ..Default::default()
        }
    }
    /// None if the currency's rate isn't configured
    pub fn from_currency(currency: Currency, count: u32) -> Option<Self> {
        CONFIG.rates[currency as usize]?;
        let mut money = Self::zero();
        money.currencies[currency as usize] = Rational32::from(count as i32);
        Some(money)
    }
    /// None if the currency has no copper value: untracked, or its rate isn't configured.
    pub fn from_api_currency(currency_id: u32, count: u32) -> Option<Self> {
        if currency_id == COIN_ID {
            return Some(Self::from_copper(count as i32));
        }
        let currency = Currency::ALL.iter().copied().find(|c| c.id() == currency_id)?;
        Self::from_currency(currency, count)
    }

    fn copper_value(&self) -> Rational32 {
        Currency::ALL.iter().copied().fold(self.copper, |value, currency| {
            let rate = CONFIG.rates[currency as usize].unwrap_or(Rational32::zero());
            value + self.currencies[currency as usize] * rate
        })
    }
    pub fn to_copper_value(&self) -> i32 {
        self.copper_value().ceil().to_integer()
    }

    fn fee(&self, percent: u8) -> Rational32 {
        cmp::max(
            Rational32::from(1),
            self.copper * Rational32::new(percent as i32, 100),
        )
        .round()
    }

    pub fn trading_post_sale_revenue(self) -> Money {
        let fees = self.fee(TRADING_POST_EXCHANGE_FEE) + self.fee(TRADING_POST_LISTING_FEE);
        Money {
            copper: if self.copper > fees {
                self.copper - fees
            } else {
                0.into()
            },
            ..Default::default()
        }
    }
    /// Has an error of at most 1 copper too high (could have broken even at one copper less)
    pub fn trading_post_listing_price(self) -> Money {
        let copper = self.copper_value();
        Money {
            copper: cmp::max(
                (copper
                    * Rational32::new(
                        100,
                        (100 - TRADING_POST_LISTING_FEE - TRADING_POST_EXCHANGE_FEE) as i32,
                    ))
                .ceil(),
                copper + 2,
            ),
            ..Default::default()
        }
    }
    /// This will probably not be precise due to rounding errors accumulating as sales are broken
    /// into smaller batches. Would need to calculate this per sale chunk to be precise - but then
    /// the TP sometimes glitches w/small sale volumes, requiring filling them multiple times
    /// anyway, reintroducing rounding errors.
    pub fn increase_by_listing_fee(self) -> Money {
        Money {
            copper: self.copper + self.fee(TRADING_POST_LISTING_FEE),
            currencies: self.currencies,
        }
    }

    // Gives an approximate ratio between two money values; for profit on cost
    pub fn percent(self, other: Self) -> f64 {
        let value = self.copper_value().to_f64().unwrap_or(0_f64);
        let other_value = other.copper_value().to_f64().unwrap_or(f64::INFINITY);
        value / other_value
    }

    fn format_copper(&self) -> String {
        let copper = self.copper.to_integer();
        let sign = if copper < 0 { "-" } else { "" };
        let copper = copper.abs();

        let gold = copper / 10000;
        let silver = (copper - gold * 10000) / 100;
        let copper = copper - gold * 10000 - silver * 100;

        format!("{}{}.{:02}.{:02}g", sign, gold, silver, copper)
    }

    /// Nonzero currencies other than coin, with their whole units needed, signed like copper
    fn currency_units(&self) -> impl Iterator<Item = (Currency, String)> + '_ {
        Currency::ALL.iter().copied().filter_map(move |currency| {
            let count = self.currencies[currency as usize];
            if count.is_zero() {
                return None;
            }
            let sign = if count < Rational32::zero() { "-" } else { "" };
            Some((currency, format!("{}{}", sign, count.abs().ceil().to_integer())))
        })
    }
}
impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut currencies = Vec::new();

        if self.copper != Rational32::zero() {
            currencies.push(self.format_copper());
        }
        for (currency, units) in self.currency_units() {
            currencies.push(format!("{} {}", units, currency.abbrev()));
        }

        write!(f, "{}", currencies.join(", "))
    }
}
impl Default for Money {
    fn default() -> Self {
        Self::zero()
    }
}
impl Zero for Money {
    fn zero() -> Self {
        Self {
            copper: Rational32::zero(),
            currencies: [Rational32::zero(); Currency::ALL.len()],
        }
    }
    fn is_zero(&self) -> bool {
        self.copper.is_zero()
    }
}
impl std::ops::Add for Money {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            copper: self.copper + other.copper,
            currencies: array::from_fn(|i| self.currencies[i] + other.currencies[i]),
        }
    }
}
impl std::ops::Sub for Money {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self {
            copper: self.copper - other.copper,
            currencies: array::from_fn(|i| self.currencies[i] - other.currencies[i]),
        }
    }
}
impl std::ops::AddAssign for Money {
    fn add_assign(&mut self, other: Self) {
        *self = *self + other;
    }
}
impl std::ops::Mul<u32> for Money {
    type Output = Self;

    fn mul(self, other: u32) -> Self {
        Self {
            copper: self.copper * other as i32,
            currencies: self.currencies.map(|count| count * other as i32),
        }
    }
}
impl std::ops::Div<u32> for Money {
    type Output = Self;

    fn div(self, other: u32) -> Self {
        Self {
            copper: self.copper / other as i32,
            currencies: self.currencies.map(|count| count / other as i32),
        }
    }
}
impl PartialEq for Money {
    fn eq(&self, other: &Self) -> bool {
        self.copper == other.copper && self.currencies == other.currencies
    }
}
impl PartialOrd for Money {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Money {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let value = self.copper_value();
        let other_value = other.copper_value();
        value.cmp(&other_value)
    }
}
impl std::iter::Sum for Money {
    fn sum<I: Iterator<Item = Money>>(iter: I) -> Self {
        let mut sink = Money::zero();
        let mut copper = Rational64::zero();
        for src in iter {
            copper += Rational64::new(
                i64::from(*src.copper.numer()),
                i64::from(*src.copper.denom()),
            );
            sink.currencies = array::from_fn(|i| sink.currencies[i] + src.currencies[i]);
        }
        let mut error = false;
        sink.copper = Rational32::new(
            i32::try_from(*copper.numer()).unwrap_or_else(|_| {
                error = true;
                i32::MAX
            }),
            i32::try_from(*copper.denom()).unwrap_or_else(|_| {
                error = true;
                i32::MAX
            }),
        );
        if error {
            eprintln!("Overflow error: Money sum will be approximate");
        }
        sink
    }
}

impl fmt::Debug for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut result = f.debug_struct("Money");

        if self.copper != Rational32::zero() {
            result.field("copper", &self.format_copper());
        }
        for (currency, units) in self.currency_units() {
            result.field(currency.abbrev(), &units);
        }

        result.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sale_revenue() {
        // NOTE: all prices verified in game; game subtracts listing fee from
        // cash and exchange fee from revenue.
        let prices = vec![(2, 0), (6, 4), (6 * 2, 10), (6 * 3, 15), (51, 43), (68, 58)];
        for (sell, revenue) in prices {
            assert_eq!(
                revenue,
                Money::from_copper(sell)
                    .trading_post_sale_revenue()
                    .to_copper_value()
            );
        }
    }

    #[test]
    fn listing_price() {
        let epsilon = Money::from_copper(1);
        // Bunch of arbitrary primes
        let values = vec![
            1, 2, 3, 17, 31, 37, 47, 53, 71, 101, 137, 3499, 9431, 100673, 199799, 1385507,
            24710753,
        ];
        for value in values {
            let price = Money::from_copper(value);
            let breakeven =
                Money::from_copper(price.trading_post_listing_price().to_copper_value())
                    .trading_post_sale_revenue();
            assert!(price <= breakeven && breakeven <= price + epsilon);
        }
    }
}
