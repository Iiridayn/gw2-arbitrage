use num_traits::Zero;
use std::collections::HashMap;

use crate::api;
use crate::item::ITEM_STACK_SIZE;
use crate::money::Money;
use crate::recipe::{Discipline::MysticForge, Recipe, INGREDIENT_SWITCH_SECONDS};

const SPARKLINE_SLICES: usize = 8;
const SPARKLINE_BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

const TP_ACTION_SECONDS: f64 = 5.0;
/// Selling is limited to about 15 per 2 minutes
const SALES_BEFORE_COOLDOWN: u32 = 15;
const SALE_COOLDOWN_SECONDS: f64 = 8.0;
/// Opening the TP on the output from the inventory
const OUTPUT_SWITCH_SECONDS: f64 = 3.0;

/// Seconds spent buying and selling. One transaction per stack bought of each TP ingredient, and
/// per stack sold into each price level, since a sale can't span prices. Sales are made while the
/// output crafts, except at crafting stations, where a stack can't be sold until it's complete.
pub fn market_seconds(
    tp_purchases: &HashMap<u32, u32>,
    vendor_purchases: usize,
    buy_order_quantities: &[u32],
    sold: u32,
    fixed_value: bool,
    recipe: Option<&Recipe>,
) -> u32 {
    let buys: u32 = tp_purchases
        .values()
        .map(|count| count.div_ceil(ITEM_STACK_SIZE))
        .sum();
    let sales: u32 = if fixed_value {
        sold.div_ceil(ITEM_STACK_SIZE)
    } else {
        // buys are sorted in ascending price
        let mut unsold = sold;
        buy_order_quantities
            .iter()
            .rev()
            .map(|&quantity| {
                let sold_at_price = quantity.min(unsold);
                unsold -= sold_at_price;
                sold_at_price.div_ceil(ITEM_STACK_SIZE)
            })
            .sum()
    };
    let selling = sales.min(SALES_BEFORE_COOLDOWN) as f64 * TP_ACTION_SECONDS
        + sales.saturating_sub(SALES_BEFORE_COOLDOWN) as f64 * SALE_COOLDOWN_SECONDS;
    let output_crafting = recipe.map_or(0.0, |recipe| {
        recipe.crafting_seconds(sold / recipe.output_item_count)
    });
    // the forge crafts one at a time, so only the last sale must wait
    let forged = recipe.is_some_and(|recipe| recipe.disciplines.contains(&MysticForge));
    let selling_after_crafting = if forged {
        TP_ACTION_SECONDS
    } else {
        let last_stack = (sold - 1) % ITEM_STACK_SIZE + 1;
        selling * last_stack as f64 / sold as f64
    };
    let seconds = buys as f64 * TP_ACTION_SECONDS
        + (tp_purchases.len() + vendor_purchases) as f64 * INGREDIENT_SWITCH_SECONDS
        + OUTPUT_SWITCH_SECONDS
        + (selling - output_crafting).max(selling_after_crafting);
    seconds.round() as u32
}

#[derive(Clone, Debug)]
pub struct ItemListings {
    pub id: u32,
    pub buys: Vec<Listing>,
    pub sells: Vec<Listing>,
    pub pending_buy_quantity: u32,
}

#[derive(Clone, Debug)]
pub struct Listing {
    pub unit_price: u32,
    pub quantity: u32,
}

impl ItemListings {
    pub fn buy(&mut self, mut count: u32) -> Option<(u32, u32, u32)> {
        let mut cost = 0;
        let mut min_sell = 0;
        let mut max_sell = 0;

        while count > 0 {
            // sells are sorted in descending price
            let remove = if let Some(listing) = self.sells.last_mut() {
                listing.quantity -= 1;
                count -= 1;
                if min_sell == 0 {
                    min_sell = listing.unit_price;
                }
                max_sell = listing.unit_price;
                cost += listing.unit_price;
                listing.quantity.is_zero()
            } else {
                return None;
            };

            if remove {
                self.sells.pop();
            }
        }

        Some((cost, min_sell, max_sell))
    }

    pub fn sell(&mut self, mut count: u32) -> Option<(Money, u32)> {
        let mut revenue = Money::zero();
        let mut min_buy = 0;

        while count > 0 {
            // buys are sorted in ascending price
            let remove = if let Some(listing) = self.buys.last_mut() {
                listing.quantity -= 1;
                count -= 1;
                min_buy = listing.unit_price;
                revenue +=
                    Money::from_copper(listing.unit_price as i32).trading_post_sale_revenue();
                listing.quantity.is_zero()
            } else {
                return None;
            };

            if remove {
                self.buys.pop();
            }
        }

        Some((revenue, min_buy))
    }

    pub fn lowest_sell_offer(&self, mut quantity: u32) -> Option<u32> {
        debug_assert!(!quantity.is_zero());

        let mut cost = 0;
        let mut pending_buy_quantity = self.pending_buy_quantity;

        for listing in self.sells.iter().rev() {
            let mut remaining_listing_quantity = listing.quantity;
            if pending_buy_quantity > 0 {
                if pending_buy_quantity >= remaining_listing_quantity {
                    pending_buy_quantity -= remaining_listing_quantity;
                    remaining_listing_quantity = 0;
                } else {
                    remaining_listing_quantity -= pending_buy_quantity;
                    pending_buy_quantity = 0;
                }
            }

            if remaining_listing_quantity > 0 {
                if remaining_listing_quantity < quantity {
                    quantity -= remaining_listing_quantity;
                    cost += remaining_listing_quantity * listing.unit_price;
                } else {
                    cost += quantity * listing.unit_price;
                    quantity = 0;
                }
            }

            if quantity.is_zero() {
                break;
            }
        }

        if quantity > 0 {
            None
        } else {
            Some(cost)
        }
    }
}

impl From<api::ItemListings> for ItemListings {
    fn from(v: api::ItemListings) -> Self {
        ItemListings {
            id: v.id,
            buys: v
                .buys
                .into_iter()
                .map(|listing| Listing {
                    unit_price: listing.unit_price,
                    quantity: listing.quantity.into(),
                })
                .collect(),
            sells: v
                .sells
                .into_iter()
                .map(|listing| Listing {
                    unit_price: listing.unit_price,
                    quantity: listing.quantity.into(),
                })
                .collect(),
            pending_buy_quantity: 0,
        }
    }
}

/// Money traded taking `count` from `listings` best price first, as bars across the price range.
/// The bar after the `|` is the value left from the last price to one slice past it, on the same
/// scale. Works on both sides of the book: buying from sells or selling into buys.
pub fn listing_sparkline(listings: &[api::Listing], count: u32) -> String {
    let mut taken = vec![];
    let mut remaining = vec![];
    let mut to_take = count;
    // listings are reversed so the best price is last
    for listing in listings.iter().rev() {
        let quantity = listing.quantity.min(to_take);
        to_take -= quantity;
        if quantity > 0 {
            taken.push((listing.unit_price, quantity));
        }
        if listing.quantity > quantity {
            remaining.push((listing.unit_price, listing.quantity - quantity));
        }
    }

    let (best, last) = match (taken.first(), taken.last()) {
        (Some(best), Some(last)) => (best.0 as f64, last.0 as f64),
        _ => return String::new(),
    };
    let range = (last - best).abs();
    // prices are whole copper, so there's no point in slices narrower than 1c
    let slices = (range as usize + 1).min(SPARKLINE_SLICES);

    let mut bars = vec![0.0; slices];
    for &(price, quantity) in &taken {
        let price = price as f64;
        let position = if range > 0.0 {
            (price - best).abs() / range
        } else {
            0.0
        };
        let slice = (position * slices as f64) as usize;
        bars[slice.min(slices - 1)] += price * quantity as f64;
    }

    let continuation_reach = (range / slices as f64).max(last * 0.01).max(1.0);
    let continuation: f64 = remaining
        .iter()
        .filter(|listing| (listing.0 as f64 - last).abs() <= continuation_reach)
        .map(|listing| listing.0 as f64 * listing.1 as f64)
        .sum();

    let tallest = bars.iter().cloned().fold(0.0, f64::max);
    let top_level = SPARKLINE_BARS.len() - 1;
    let bar = |value: f64| {
        let level = (value / tallest * top_level as f64).round() as usize;
        SPARKLINE_BARS[level.min(top_level)]
    };

    let mut sparkline: String = bars.iter().map(|&value| bar(value)).collect();
    sparkline.push('|');
    sparkline.push(bar(continuation));
    sparkline
}
