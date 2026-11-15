use num_rational::Rational32;
use num_traits::Zero;

use std::array;
use std::cmp;
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
    pub const ALL: [Currency; 5] = [
        Karma,
        UnboundMagic,
        VolatileMagic,
        ResearchNote,
        SpiritShard,
    ];

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

/// Rounds up, so the first of several shares is the largest
fn div_ceil(n: i64, d: i64) -> i64 {
    -(-n).div_euclid(d)
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct Money {
    copper: i32,
    // indexed by `Currency as usize`
    currencies: [i32; Currency::ALL.len()],
    value: i32, // copper, with currencies at their rates; split apart from them, so shares are fair
}
impl Money {
    pub fn from_copper(copper: i32) -> Self {
        Self {
            copper,
            value: copper,
            ..Default::default()
        }
    }
    /// None if the currency's rate isn't configured
    pub fn from_currency(currency: Currency, count: u32) -> Option<Self> {
        Some(Self::at_rate(
            currency,
            count,
            CONFIG.rates[currency as usize]?,
        ))
    }
    fn at_rate(currency: Currency, count: u32, rate: Rational32) -> Self {
        let mut money = Self::zero();
        money.currencies[currency as usize] = count as i32;
        money.value = (rate * count as i32).ceil().to_integer();
        money
    }
    /// Splits this between `count` of `total` items and the rest; the share is rounded up, and the
    /// two always add up to this
    pub fn split(self, count: u32, total: u32) -> (Self, Self) {
        let share = |amount: i32| div_ceil(amount as i64 * count as i64, total as i64) as i32;
        let share = Self {
            copper: share(self.copper),
            currencies: self.currencies.map(share),
            value: share(self.value),
        };
        (share, self - share)
    }
    /// None if the currency has no copper value: untracked, or its rate isn't configured.
    pub fn from_api_currency(currency_id: u32, count: u32) -> Option<Self> {
        if currency_id == COIN_ID {
            return Some(Self::from_copper(count as i32));
        }
        let currency = Currency::ALL
            .iter()
            .copied()
            .find(|c| c.id() == currency_id)?;
        Self::from_currency(currency, count)
    }

    pub fn to_copper_value(&self) -> i32 {
        self.value
    }

    fn fee(&self, percent: u8) -> i32 {
        cmp::max(
            Rational32::from(1),
            Rational32::new(percent as i32, 100) * self.copper,
        )
        .round()
        .to_integer()
    }

    pub fn trading_post_sale_revenue(self) -> Money {
        let fees = self.fee(TRADING_POST_EXCHANGE_FEE) + self.fee(TRADING_POST_LISTING_FEE);
        Money::from_copper(if self.copper > fees {
            self.copper - fees
        } else {
            0
        })
    }
    /// What the TP pays out for a sale; the listing fee is taken from the wallet when listing
    pub fn trading_post_payout(self) -> Money {
        Money::from_copper((self.copper - self.fee(TRADING_POST_EXCHANGE_FEE)).max(0))
    }
    /// Has an error of at most 1 copper too high (could have broken even at one copper less)
    pub fn trading_post_listing_price(self) -> Money {
        let copper = Rational32::from(self.value);
        Money::from_copper(
            cmp::max(
                (copper
                    * Rational32::new(
                        100,
                        (100 - TRADING_POST_LISTING_FEE - TRADING_POST_EXCHANGE_FEE) as i32,
                    ))
                .ceil(),
                copper + 2,
            )
            .ceil()
            .to_integer(),
        )
    }
    /// Paid from the wallet when listing a sale of this
    pub fn trading_post_listing_fee(self) -> Money {
        Money::from_copper(self.fee(TRADING_POST_LISTING_FEE))
    }

    /// Gives an approximate ratio between two money values; for profit on cost
    pub fn percent(self, other: Self) -> f64 {
        self.value as f64 / other.value as f64
    }

    fn format_copper(&self) -> String {
        let copper = self.copper;
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
            let sign = if count < 0 { "-" } else { "" };
            Some((currency, format!("{}{}", sign, count.abs())))
        })
    }

    /// Coin and other currencies as separate strings, each empty when zero; lets tables align coin
    pub fn split_display(&self) -> (String, String) {
        let coin = if self.copper.is_zero() {
            String::new()
        } else {
            self.format_copper()
        };
        let currencies: Vec<String> = self
            .currency_units()
            .map(|(currency, units)| format!("{} {}", units, currency.abbrev()))
            .collect();
        (coin, currencies.join(", "))
    }
}
impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (coin, currencies) = self.split_display();
        let sep = if coin.is_empty() || currencies.is_empty() {
            ""
        } else {
            ", "
        };
        write!(f, "{}{}{}", coin, sep, currencies)
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
            copper: 0,
            currencies: [0; Currency::ALL.len()],
            value: 0,
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
            value: self.value + other.value,
        }
    }
}
impl std::ops::Sub for Money {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self {
            copper: self.copper - other.copper,
            currencies: array::from_fn(|i| self.currencies[i] - other.currencies[i]),
            value: self.value - other.value,
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
            value: self.value * other as i32,
        }
    }
}
/// Rounds up
impl std::ops::Div<u32> for Money {
    type Output = Self;

    fn div(self, other: u32) -> Self {
        let (share, _) = self.split(1, other);
        share
    }
}
impl PartialOrd for Money {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Money {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}
impl std::iter::Sum for Money {
    fn sum<I: Iterator<Item = Money>>(iter: I) -> Self {
        iter.fold(Money::zero(), |sum, money| sum + money)
    }
}

impl fmt::Debug for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut result = f.debug_struct("Money");

        if self.copper != 0 {
            result.field("copper", &self.format_copper());
        }
        for (currency, units) in self.currency_units() {
            result.field(currency.abbrev(), &units);
        }
        if self.value != self.copper {
            result.field("value", &self.value);
        }

        result.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fees_are_charged_on_a_sales_total() {
        // verified in game: (count, unit price, listing fee, exchange fee); each fee is rounded to
        // the nearest copper on the total, halves up, at least 1
        let sales = [
            (10, 93, 47, 93),
            (3, 70, 11, 21),
            (5, 187, 47, 94),
            (7, 13, 5, 9),
            (3, 7, 1, 2),
        ];
        for (count, price, listing_fee, exchange_fee) in sales {
            let total = Money::from_copper(count * price);
            let copper = |money: Money| money.to_copper_value();
            assert_eq!(copper(total.trading_post_listing_fee()), listing_fee);
            assert_eq!(copper(total - total.trading_post_payout()), exchange_fee);
            let revenue = count * price - listing_fee - exchange_fee;
            assert_eq!(copper(total.trading_post_sale_revenue()), revenue);
        }
    }

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

    #[test]
    fn split_keeps_currencies_whole_and_shares_value_fairly() {
        let shard = Money::at_rate(SpiritShard, 1, Rational32::from(5000));
        let (first, rest) = (Money::from_copper(100) + shard).split(1, 10);
        assert_eq!(
            first,
            Money {
                value: 510,
                ..Money::from_copper(10) + shard
            }
        );
        assert_eq!(
            rest,
            Money {
                value: 4590,
                ..Money::from_copper(90)
            }
        );
        assert!(first < rest);
    }

    #[test]
    fn currency_value_rounds_up() {
        let karma = Money::at_rate(Karma, 7, Rational32::new(9, 10));
        assert_eq!(karma.to_copper_value(), 7);
    }
}
