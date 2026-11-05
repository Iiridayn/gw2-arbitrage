use rayon::prelude::*;

use num_traits::Zero;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::api;
use crate::config;
use crate::crafting;
use crate::item::Item;
use crate::money::Money;
use crate::recipe::Recipe;
use crate::request;
use crate::trading_post::{market_seconds, ItemListings};
use config::CONFIG;

/// Return a items which are profitable to make at least one of, and their ingredients, for further
/// scrutiny
pub fn find_profitable_items(
    tp_prices_map: &HashMap<u32, api::Price>,
    recipes_map: &HashMap<u32, Recipe>,
    items_map: &HashMap<u32, Item>,
) -> (Vec<u32>, Vec<u32>) {
    let mut profitable_item_ids = vec![];
    let mut ingredient_ids = vec![];
    for (item_id, recipe) in recipes_map {
        if let Some(item) = items_map.get(item_id) {
            // we cannot sell restricted items
            if item.is_restricted() {
                continue;
            }
        }

        if let Some(filter_disciplines) = &CONFIG.filter_disciplines {
            let mut has_discipline = false;
            for discipline in filter_disciplines {
                if recipe.disciplines.iter().any(|s| s == discipline) {
                    has_discipline = true;
                    break;
                }
            }

            if !has_discipline {
                continue;
            }
        }

        // some items are craftable and have no listed restrictions but are still not listable on tp
        // e.g. 39417, 79557
        // conversely, some items have a NoSell flag but are listable on the trading post
        // e.g. 66917
        let tp_prices = match tp_prices_map.get(item_id) {
            Some(tp_prices) if tp_prices.sells.quantity > 0 => tp_prices,
            _ => continue,
        };

        if let Some(crafting::EstimatedCraftingCost {
            source: crafting::Source::Crafting,
            cost: crafting_cost,
        }) = crafting::calculate_estimated_min_crafting_cost(
            *item_id,
            recipes_map,
            items_map,
            tp_prices_map,
            None,
            &CONFIG.crafting,
        ) {
            let effective_buy_price =
                Money::from_copper(tp_prices.buys.unit_price as i32).trading_post_sale_revenue();
            if effective_buy_price > crafting_cost {
                profitable_item_ids.push(*item_id);
                if let Some(recipe) = recipes_map.get(item_id) {
                    recipe.collect_ingredient_ids(recipes_map, &mut ingredient_ids);
                }
            }
        }
    }

    (profitable_item_ids, ingredient_ids)
}

/// Compute exact profit of profitable items independently in parallel
pub fn profitable_item_list(
    tp_listings_map: &HashMap<u32, api::ItemListings>,
    profitable_item_ids: &Vec<u32>,
    request_listing_item_ids: &[u32],
    recipes_map: &HashMap<u32, Recipe>,
    items_map: &HashMap<u32, Item>,
    tp_prices_map: &HashMap<u32, api::Price>,
    unlocks: &crafting::RecipeUnlocks,
) -> Vec<(ProfitableItem, crafting::RequiredUnlocks)> {
    let profitable_items: Vec<_> = profitable_item_ids
        .par_iter()
        .filter_map(|item_id| {
            let mut ingredient_ids = vec![*item_id];
            if let Some(recipe) = recipes_map.get(item_id) {
                recipe.collect_ingredient_ids(recipes_map, &mut ingredient_ids);
            }

            let mut tp_listings_map_for_item: HashMap<u32, _> = HashMap::new();
            for id in ingredient_ids {
                debug_assert!(request_listing_item_ids.contains(&id));
                if let Some(listing) = tp_listings_map.get(&id).cloned() {
                    tp_listings_map_for_item.insert(id, listing);
                }
            }

            calculate_crafting_profit(
                *item_id,
                recipes_map,
                items_map,
                &tp_listings_map_for_item,
                None,
                &CONFIG.crafting,
            )
        })
        .collect();
    let mut profitable_items: Vec<_> = profitable_items
        .into_iter()
        .filter_map(|item| charge_unlocks(item, unlocks, recipes_map, items_map, tp_prices_map))
        .collect();

    match CONFIG.sort {
        config::SortBy::Total => profitable_items.sort_unstable_by_key(|(item, _)| item.profit),
        config::SortBy::Hour => {
            profitable_items.sort_by_cached_key(|(item, _)| item.profit_per_hour())
        }
        config::SortBy::Roi => profitable_items
            .sort_by(|(a, _), (b, _)| a.profit_on_cost().total_cmp(&b.profit_on_cost())),
    }

    profitable_items
}

pub async fn calc_item_profit(
    item_id: u32,
    recipes_map: &HashMap<u32, Recipe>,
    items_map: &HashMap<u32, Item>,
    unlocks: &crafting::RecipeUnlocks<'_>,
    notify: Option<&dyn Fn(&str)>,
) -> Result<
    (
        Option<(ProfitableItem, crafting::RequiredUnlocks)>,
        HashMap<(u32, crafting::Source), crafting::PurchasedIngredient>,
        HashMap<u32, api::ItemListings>,
    ),
    Box<dyn std::error::Error>,
> {
    let mut items_to_price = vec![];
    if let Some(recipe) = recipes_map.get(&item_id) {
        recipe.collect_ingredient_ids(recipes_map, &mut items_to_price);
    }

    let mut request_listing_item_ids = vec![item_id];
    request_listing_item_ids.extend(items_to_price);
    request_listing_item_ids.sort_unstable();
    request_listing_item_ids.dedup();

    let tp_listings =
        request::fetch_item_listings(&request_listing_item_ids, Some(&CONFIG.cache_dir), notify)
            .await?;
    let tp_listings_map = vec_to_map(tp_listings, |x| x.id);

    let mut purchased_ingredients = Default::default();
    let profitable_item = calculate_crafting_profit(
        item_id,
        recipes_map,
        items_map,
        &tp_listings_map,
        Some(&mut purchased_ingredients),
        &CONFIG.crafting,
    );

    let profitable_item = match profitable_item {
        Some(item)
            if !item
                .crafted_items
                .unknown_recipes(recipes_map, unlocks.known_recipes)
                .is_empty() =>
        {
            // Unlock items and their crafting ingredients aren't known before crafting is decided
            let tp_prices: Vec<api::Price> =
                request::request_paginated("commerce/prices", &None, None).await?;
            let tp_prices_map = vec_to_map(tp_prices, |x| x.id);
            charge_unlocks(item, unlocks, recipes_map, items_map, &tp_prices_map)
        }
        Some(item) => Some((item, Default::default())),
        None => None,
    };

    Ok((profitable_item, purchased_ingredients, tp_listings_map))
}

/// Find the unlock items an item's recipes need, charge them to the item, and return None if it's
/// then no longer worth crafting.
fn charge_unlocks(
    mut item: ProfitableItem,
    unlocks: &crafting::RecipeUnlocks,
    recipes_map: &HashMap<u32, Recipe>,
    items_map: &HashMap<u32, Item>,
    tp_prices_map: &HashMap<u32, api::Price>,
) -> Option<(ProfitableItem, crafting::RequiredUnlocks)> {
    let required = unlocks.required(
        &item.crafted_items,
        recipes_map,
        items_map,
        tp_prices_map,
        &CONFIG.crafting,
    );
    // Can no longer be learned: Wooden Sword, Asuran Harpoon
    if required.contains_key(&5424) || required.contains_key(&6057) {
        return None;
    }
    // keyed by unlock item, since one can unlock several recipes
    let costs: HashMap<u32, Money> = required.values().flatten().copied().collect();
    let cost: Money = costs.into_values().sum();
    if !cost.is_zero() {
        item.profit = item.profit - cost;
        item.crafting_cost += cost;
        let below_rate = CONFIG
            .crafting
            .min_per_hour
            .is_some_and(|gold| item.profit_per_hour() <= Money::from_copper(gold as i32 * 10000));
        if item.profit <= Money::zero() || below_rate {
            return None;
        }
    }
    Some((item, required))
}

pub fn calculate_crafting_profit(
    item_id: u32,
    recipes_map: &HashMap<u32, Recipe>,
    items_map: &HashMap<u32, Item>,
    tp_listings_map: &HashMap<u32, api::ItemListings>,
    mut purchased_ingredients: Option<
        &mut HashMap<(u32, crafting::Source), crafting::PurchasedIngredient>,
    >,
    opt: &config::CraftingOptions,
) -> Option<ProfitableItem> {
    let api_listings_map = tp_listings_map;
    let buy_order_quantities: Vec<u32> = tp_listings_map.get(&item_id).map_or_else(Vec::new, |l| {
        l.buys.iter().map(|listing| listing.quantity).collect()
    });

    let mut tp_listings_map: BTreeMap<u32, ItemListings> = tp_listings_map
        .clone()
        .into_iter()
        .map(|(id, listings)| (id, ItemListings::from(listings)))
        .collect();

    let recipe = recipes_map.get(&item_id);
    let output_item_count = recipe.map(|recipe| recipe.output_item_count).unwrap_or(1);
    let min_profit = Money::from_copper(opt.min_per_item.unwrap_or(0) as i32) * output_item_count;

    let mut listing_profit = Money::zero();
    let mut total_crafting_cost = Money::zero();
    let mut crafting_count: u32 = 0;
    let mut crafted_items = crafting::CraftedItems::default();
    let mut tp_purchases: HashMap<u32, u32> = HashMap::new();
    let mut vendor_purchases: HashSet<u32> = HashSet::new();
    let mut best_score = 0.0;
    let mut best_count = 0;

    let mut min_sell = 0;
    let max_sell = tp_listings_map.get(&item_id).map_or_else(
        || opt.value.unwrap_or(0),
        |listings| listings.buys.last().map_or(0, |l| l.unit_price),
    );
    let mut breakeven = Money::zero();

    // simulate crafting 1 item per loop iteration until it becomes unprofitable
    loop {
        if let Some(count) = opt.count {
            if crafting_count + output_item_count > count {
                break;
            }
        }

        let mut context = crafting::PreciseCraftingCostContext {
            purchases: vec![],
            items: crafted_items.clone(),
        };

        let crafting_cost = if let Some(crafting::PreciseCraftingCost {
            source: crafting::Source::Crafting,
            cost,
        }) = crafting::calculate_precise_min_crafting_cost(
            item_id,
            output_item_count,
            recipes_map,
            items_map,
            &mut tp_listings_map,
            &mut context,
            opt,
        ) {
            cost
        } else {
            break;
        };

        let (buy_price, min_buy) = if let Some(price) = opt.value {
            (Money::from_copper(price as i32) * output_item_count, price)
        } else if let Some((buy_price, min_buy)) = tp_listings_map
            .get_mut(&item_id)
            .unwrap_or_else(|| panic!("Missing listings for item id: {}", item_id))
            .sell(output_item_count)
        {
            (buy_price, min_buy)
        } else {
            break;
        };

        // Ensure buy_price is larger before subtracting cost for profit
        if buy_price < crafting_cost + min_profit {
            break;
        }

        listing_profit += buy_price - crafting_cost;
        total_crafting_cost += crafting_cost;
        crafting_count += output_item_count;
        crafted_items = context.items;

        min_sell = min_buy;
        // Breakeven is based on the last/most expensive to craft
        breakeven = crafting_cost / output_item_count;

        // Finalize purchases
        for (purchase_id, count, purchase_source) in &context.purchases {
            let (cost, min_sell, max_sell) = if let crafting::Source::TradingPost = *purchase_source
            {
                let listing = tp_listings_map.get_mut(purchase_id).unwrap_or_else(|| {
                    panic!(
                        "Missing listings for ingredient {} of item id {}",
                        purchase_id, item_id
                    )
                });
                listing.pending_buy_quantity -= *count;
                let (cost, min_sell, max_sell) = listing.buy(*count).unwrap_or_else(|| {
                    panic!(
                        "Expected to be able to buy {} of ingredient {} for item id {}",
                        count, purchase_id, item_id
                    )
                });
                *tp_purchases.entry(*purchase_id).or_insert(0) += *count;
                (cost, min_sell, max_sell)
            } else {
                if let crafting::Source::Vendor = *purchase_source {
                    vendor_purchases.insert(*purchase_id);
                }
                (0, 0, 0)
            };

            if let Some(purchased_ingredients) = &mut purchased_ingredients {
                let ingredient = purchased_ingredients
                    .entry((*purchase_id, *purchase_source))
                    .or_insert_with(|| crafting::PurchasedIngredient {
                        count: 0,
                        max_price: Money::default(),
                        min_price: Money::default(),
                        total_cost: Money::default(),
                    });
                ingredient.count += count;
                if ingredient.min_price.is_zero() {
                    ingredient.min_price = Money::from_copper(min_sell as i32);
                }
                ingredient.max_price = Money::from_copper(max_sell as i32);
                ingredient.total_cost += Money::from_copper(cost as i32);
            }
        }
        debug_assert!(tp_listings_map
            .iter()
            .all(|(_, listing)| listing.pending_buy_quantity == 0));

        let (leftover_cost, leftover_revenue) =
            value_leftovers(&crafted_items.leftovers, &tp_listings_map);
        let mut score =
            (listing_profit + leftover_revenue - leftover_cost).to_copper_value() as f64;
        if let Some(gold) = opt.min_per_hour {
            let seconds = crafted_items.crafting_seconds(item_id, recipes_map)
                + market_seconds(
                    &tp_purchases,
                    vendor_purchases.len(),
                    &buy_order_quantities,
                    crafting_count,
                    opt.value.is_some(),
                    recipe,
                ) as f64;
            score -= gold as f64 * 10000.0 * seconds / 3600.0;
        }
        if score > best_score {
            best_score = score;
            best_count = crafting_count;
        }
    }

    // Stop at the most profitable count, after selling leftovers and, with --min-per-hour, the
    // time spent
    if best_count == 0 {
        return None;
    }
    if best_count < crafting_count {
        if let Some(purchased_ingredients) = &mut purchased_ingredients {
            purchased_ingredients.clear();
        }
        return calculate_crafting_profit(
            item_id,
            recipes_map,
            items_map,
            api_listings_map,
            purchased_ingredients,
            &config::CraftingOptions {
                count: Some(best_count),
                min_per_hour: None,
                ..*opt
            },
        );
    }

    let (leftover_cost, leftover_revenue) =
        value_leftovers(&crafted_items.leftovers, &tp_listings_map);
    Some(ProfitableItem {
        id: item_id,
        crafting_cost: total_crafting_cost + leftover_cost,
        profit: listing_profit + leftover_revenue - leftover_cost,
        count: crafting_count,
        max_sell: Money::from_copper(max_sell as i32),
        min_sell: Money::from_copper(min_sell as i32),
        breakeven: breakeven.trading_post_listing_price(),
        crafting_steps: crafted_items.crafting_steps(recipes_map).to_integer(),
        crafting_seconds: crafted_items.crafting_seconds(item_id, recipes_map).round() as u32,
        market_seconds: market_seconds(
            &tp_purchases,
            vendor_purchases.len(),
            &buy_order_quantities,
            crafting_count,
            opt.value.is_some(),
            recipe,
        ),
        crafted_items,
    })
}

/// Cost of the leftovers, and their revenue sold into buy orders, at most their cost; nothing if
/// there are none, e.g. account bound items
fn value_leftovers(
    leftovers: &HashMap<u32, (u32, Money, crafting::Source)>,
    tp_listings_map: &BTreeMap<u32, ItemListings>,
) -> (Money, Money) {
    let mut cost = Money::zero();
    let mut revenue = Money::zero();
    for (id, &(count, cost_per_item, _)) in leftovers {
        cost += cost_per_item * count;
        if let Some(listings) = tp_listings_map.get(id) {
            revenue += listings.sale_revenue(count).min(cost_per_item * count);
        }
    }
    (cost, revenue)
}

#[derive(Debug, Eq, PartialEq)]
pub struct ProfitableItem {
    pub id: u32,
    pub crafting_cost: Money,
    pub count: u32,
    pub profit: Money,
    pub max_sell: Money,
    pub min_sell: Money,
    pub breakeven: Money,
    pub crafting_steps: u32,
    pub crafting_seconds: u32,
    pub market_seconds: u32,
    pub crafted_items: crafting::CraftedItems,
}

impl ProfitableItem {
    pub fn profit_per_item(&self) -> Money {
        self.profit / self.count
    }

    pub fn profit_per_crafting_step(&self) -> Money {
        self.profit / self.crafting_steps
    }

    pub fn profit_per_hour(&self) -> Money {
        let seconds = (self.crafting_seconds + self.market_seconds) as f64;
        Money::from_copper((self.profit.to_copper_value() as f64 * 3600.0 / seconds) as i32)
    }

    pub fn profit_on_cost(&self) -> f64 {
        self.profit.percent(self.crafting_cost)
    }
}

pub fn vec_to_map<T, F>(v: Vec<T>, id_fn: F) -> HashMap<u32, T>
where
    F: Fn(&T) -> u32,
{
    let mut map = HashMap::default();
    for x in v.into_iter() {
        map.insert(id_fn(&x), x);
    }
    map
}
