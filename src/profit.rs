use rayon::prelude::*;

use num_traits::Zero;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::api;
use crate::config;
use crate::crafting;
use crate::money::Money;
use crate::request;
use crate::trading_post::{market_seconds, ItemListings};
use config::CONFIG;

/// Return items which are profitable to make at least one of, for further scrutiny
pub fn find_profitable_items(
    prices: &crafting::SummaryPrices,
    game_data: &crafting::GameData,
) -> Vec<u32> {
    let mut profitable_item_ids = vec![];
    for (item_id, recipes) in &game_data.recipes {
        if let Some(item) = game_data.items.get(item_id) {
            // we cannot sell restricted items
            if item.is_restricted() {
                continue;
            }
        }

        if let Some(filter_disciplines) = &CONFIG.filter_disciplines {
            let mut has_discipline = false;
            for discipline in filter_disciplines {
                if recipes
                    .iter()
                    .any(|recipe| recipe.disciplines.contains(discipline))
                {
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
        let tp_prices = match prices.tp_prices.get(item_id) {
            Some(tp_prices) if tp_prices.sells.quantity > 0 => tp_prices,
            _ => continue,
        };

        if let Some(crafting::EstimatedCraftingCost {
            source: crafting::Source::Crafting,
            cost: crafting_cost,
        }) = crafting::calculate_estimated_min_crafting_cost(
            *item_id,
            1,
            game_data,
            prices,
            &mut vec![],
            &CONFIG.crafting,
        ) {
            let effective_buy_price =
                Money::from_copper(tp_prices.buys.unit_price as i32).trading_post_sale_revenue();
            if effective_buy_price > crafting_cost {
                profitable_item_ids.push(*item_id);
            }
        }
    }

    profitable_item_ids
}

/// Compute exact profit of profitable items independently in parallel
pub fn profitable_item_list(
    tp_listings_map: &HashMap<u32, api::ItemListings>,
    profitable_item_ids: &Vec<u32>,
    request_listing_item_ids: &[u32],
    game_data: &crafting::GameData,
    known_recipes: &HashSet<u32>,
) -> Vec<ProfitableItem> {
    let mut profitable_items: Vec<_> = profitable_item_ids
        .par_iter()
        .filter_map(|item_id| {
            let mut tp_listings_map_for_item: HashMap<u32, _> = HashMap::new();
            for id in game_data.collect_tree_ids(*item_id, Some(known_recipes)) {
                debug_assert!(request_listing_item_ids.contains(&id));
                if let Some(listing) = tp_listings_map.get(&id).cloned() {
                    tp_listings_map_for_item.insert(id, listing);
                }
            }

            calculate_capped_profit(
                *item_id,
                game_data,
                &tp_listings_map_for_item,
                None,
                Some(known_recipes),
                &CONFIG.crafting,
            )
            .ok()
            .filter(is_worth_crafting)
        })
        .collect();

    profitable_items.sort_unstable_by_key(|item| item.id);
    match CONFIG.sort {
        config::SortBy::Total => profitable_items.sort_by_key(|item| item.profit),
        config::SortBy::Hour => profitable_items.sort_by_cached_key(|item| item.profit_per_hour()),
        config::SortBy::Roi => {
            profitable_items.sort_by(|a, b| a.profit_on_cost().total_cmp(&b.profit_on_cost()))
        }
        config::SortBy::Unit => profitable_items.sort_by_key(|item| item.profit_per_item()),
    }

    profitable_items
}

pub async fn calc_item_profit(
    item_id: u32,
    game_data: &crafting::GameData,
    known_recipes: &HashSet<u32>,
    notify: Option<&dyn Fn(&str)>,
) -> Result<
    (
        Result<ProfitableItem, String>,
        HashMap<(u32, crafting::Source), crafting::PurchasedIngredient>,
        HashMap<u32, api::ItemListings>,
    ),
    Box<dyn std::error::Error>,
> {
    let mut request_listing_item_ids = game_data.collect_tree_ids(item_id, Some(known_recipes));
    request_listing_item_ids.sort_unstable();

    let tp_listings =
        request::fetch_item_listings(&request_listing_item_ids, Some(&CONFIG.cache_dir), notify)
            .await?;
    let tp_listings_map = vec_to_map(tp_listings, |x| x.id);

    let mut purchased_ingredients = Default::default();
    let profitable_item = calculate_capped_profit(
        item_id,
        game_data,
        &tp_listings_map,
        Some(&mut purchased_ingredients),
        Some(known_recipes),
        &CONFIG.crafting,
    );
    let profitable_item = profitable_item.and_then(|item| {
        if is_worth_crafting(&item) {
            Ok(item)
        } else {
            Err("Not profitable after buying its recipes".to_string())
        }
    });

    Ok((profitable_item, purchased_ingredients, tp_listings_map))
}

/// Whether it's still profitable, and over --min-per-hour, after paying for its unlocks
fn is_worth_crafting(item: &ProfitableItem) -> bool {
    let mut unlocks = item.crafted_items.unlocks.values().flatten();
    // costs no more than the plan the profit loop accepted, so its checks stand
    if unlocks.all(|(_, cost)| cost.is_zero()) {
        return true;
    }
    let min_rate = CONFIG
        .crafting
        .min_per_hour
        .map(|gold| Money::from_copper(gold as i32 * 10000));
    item.profit > Money::zero() && min_rate.is_none_or(|rate| item.profit_per_hour() > rate)
}

/// The most profitable plan, adding one cap at a time while one helps, biggest possible saving
/// first. A cap either bans the recipes an unlock item teaches, to save buying it, or limits a
/// recipe to the batches the plan uses, to save the loss on its leftovers.
pub fn calculate_capped_profit(
    item_id: u32,
    game_data: &crafting::GameData,
    tp_listings_map: &HashMap<u32, api::ItemListings>,
    purchased_ingredients: Option<
        &mut HashMap<(u32, crafting::Source), crafting::PurchasedIngredient>,
    >,
    known_recipes: Option<&HashSet<u32>>,
    opt: &config::CraftingOptions,
) -> Result<ProfitableItem, String> {
    let listings: BTreeMap<u32, ItemListings> = tp_listings_map
        .iter()
        .map(|(&id, listings)| (id, ItemListings::from(listings.clone())))
        .collect();

    // scored as the profit loop scores counts
    let plan = |opt: &config::CraftingOptions| -> Result<(ProfitableItem, f64), String> {
        let item = calculate_crafting_profit(
            item_id,
            game_data,
            tp_listings_map,
            None,
            known_recipes,
            opt,
        )?;
        let mut score = item.profit.to_copper_value() as f64;
        if let Some(gold) = opt.min_per_hour {
            let seconds = (item.crafting_seconds + item.market_seconds) as f64;
            score -= gold as f64 * 10000.0 * seconds / 3600.0;
        }
        Ok((item, score))
    };

    // each cap with what it could save: banning every recipe an unlock item teaches saves its
    // cost, and capping a recipe at the batches used saves the loss on its leftovers
    let candidates = |item: &ProfitableItem| {
        const BAN: u32 = 0;
        let mut bans_by_unlock_item = BTreeMap::new();
        let mut candidates = vec![];
        for (&(id, index), &count) in &item.crafted_items.crafted {
            let recipe = &game_data.recipes[&id][index];
            let unlocks = &item.crafted_items.unlocks;
            let unlock = recipe.id.and_then(|recipe_id| *unlocks.get(&recipe_id)?);
            if let Some((unlock_id, unlock_cost)) = unlock {
                let entry = bans_by_unlock_item.entry(unlock_id);
                let (_, bans) = entry.or_insert((unlock_cost, vec![]));
                bans.push(((id, index), BAN));
            }

            if let Some(&leftover @ (left, _, crafting::Source::Crafting)) =
                item.crafted_items.leftovers.get(&id)
            {
                let (cost, revenue) = value_leftovers(&HashMap::from([(id, leftover)]), &listings);
                let batch = recipe.output_item_count;
                let batches_used = count.saturating_sub(left) / batch;
                candidates.push((cost - revenue, vec![((id, index), batches_used * batch)]));
            }
        }
        candidates.extend(bans_by_unlock_item.into_values());
        // biggest saving first; ties go by the caps, as `crafted`'s order differs between runs
        candidates.sort_by(|a, b| b.cmp(a));
        candidates
    };

    let mut opt = opt.clone();
    let (mut best, mut best_score) = plan(&opt)?;
    'search: loop {
        for (_, caps) in candidates(&best) {
            let mut capped = opt.clone();
            capped.caps.extend(caps);
            if let Ok((item, score)) = plan(&capped) {
                if score > best_score {
                    (best, best_score, opt) = (item, score, capped);
                    continue 'search;
                }
            }
        }
        break;
    }

    if purchased_ingredients.is_some() {
        calculate_crafting_profit(
            item_id,
            game_data,
            tp_listings_map,
            purchased_ingredients,
            known_recipes,
            &opt,
        )?;
    }
    Ok(best)
}

pub fn calculate_crafting_profit(
    item_id: u32,
    game_data: &crafting::GameData,
    tp_listings_map: &HashMap<u32, api::ItemListings>,
    mut purchased_ingredients: Option<
        &mut HashMap<(u32, crafting::Source), crafting::PurchasedIngredient>,
    >,
    known_recipes: Option<&HashSet<u32>>,
    opt: &config::CraftingOptions,
) -> Result<ProfitableItem, String> {
    let api_listings_map = tp_listings_map;
    let buy_order_quantities: Vec<u32> = tp_listings_map.get(&item_id).map_or_else(Vec::new, |l| {
        l.buys.iter().map(|listing| listing.quantity).collect()
    });

    let mut tp_listings_map: BTreeMap<u32, ItemListings> = tp_listings_map
        .clone()
        .into_iter()
        .map(|(id, listings)| (id, ItemListings::from(listings)))
        .collect();

    let ids = game_data.collect_tree_ids(item_id, known_recipes);
    let lowest_sells = tp_listings_map.iter().filter_map(|(&id, listings)| {
        Some((
            id,
            Money::from_copper(listings.lowest_sell_offer(1)? as i32),
        ))
    });
    let lower_bounds = crafting::lower_bounds(lowest_sells.collect(), ids.into_iter(), game_data);
    let output = |items: &crafting::CraftedItems| {
        let crafted = items.crafted.iter().filter(|((id, _), _)| *id == item_id);
        let output = crafted.map(|(&(id, index), &count)| {
            let recipe = &game_data.recipes[&id][index];
            (recipe, count / recipe.output_item_count)
        });
        output.collect::<Vec<_>>()
    };
    let min_profit = Money::from_copper(opt.min_per_item.unwrap_or(0) as i32);

    let mut listing_profit = Money::zero();
    let mut total_crafting_cost = Money::zero();
    let mut crafting_count: u32 = 0;
    let mut crafted_items = crafting::CraftedItems::default();
    let mut lots = HashMap::new();
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
    // why the loop stopped
    let stop: String;

    // simulate crafting 1 item per loop iteration until it becomes unprofitable; the rest of a
    // batch is taken from leftovers
    loop {
        if opt.count.is_some_and(|count| crafting_count >= count) {
            stop = "Reached --count".to_string();
            break;
        }

        let mut context = crafting::PreciseCraftingCostContext {
            purchases: vec![],
            items: crafted_items.clone(),
            being_crafted: vec![],
            lots: lots.clone(),
            seconds: 0.0,
        };

        let crafting_cost = match crafting::calculate_precise_min_crafting_cost(
            item_id,
            1,
            game_data,
            &mut tp_listings_map,
            &lower_bounds,
            &mut context,
            opt,
        ) {
            Some(crafting::PreciseCraftingCost {
                source: crafting::Source::Crafting,
                cost,
                ..
            }) => cost,
            Some(crafting::PreciseCraftingCost {
                cost,
                source,
                crafting: Some(crafting),
            }) => {
                stop = format!(
                    "Cheaper to buy {} ({} each) than to craft ({} each)",
                    if source == crafting::Source::Vendor {
                        "from a vendor"
                    } else {
                        "on the TP"
                    },
                    cost,
                    crafting
                );
                break;
            }
            _ => {
                stop = "An ingredient can't be bought or crafted".to_string();
                break;
            }
        };

        let (buy_price, min_buy) = if let Some(price) = opt.value {
            (Money::from_copper(price as i32), price)
        } else if let Some((buy_price, min_buy)) = tp_listings_map
            .get_mut(&item_id)
            .unwrap_or_else(|| panic!("Missing listings for item id: {}", item_id))
            .sell(1)
        {
            (buy_price, min_buy)
        } else {
            stop = "Not enough buy orders".to_string();
            break;
        };

        // Ensure buy_price is larger before subtracting cost for profit
        if buy_price < crafting_cost + min_profit {
            stop = format!(
                "Crafting costs {} each, {} ({} each)",
                crafting_cost,
                if buy_price < crafting_cost {
                    "more than it sells for"
                } else {
                    "too close to its sale price for --min-per-item"
                },
                buy_price
            );
            break;
        }

        listing_profit += buy_price - crafting_cost;
        total_crafting_cost += crafting_cost;
        crafting_count += 1;
        crafted_items = context.items;
        lots = context.lots;

        min_sell = min_buy;
        breakeven = crafting_cost; // minimum to recoup crafting costs

        release_pending(&context.purchases, &mut tp_listings_map);
        finalize_purchases(
            &context.purchases,
            &mut tp_listings_map,
            &mut tp_purchases,
            &mut vendor_purchases,
            purchased_ingredients.as_deref_mut(),
            item_id,
        );
        debug_assert!(tp_listings_map
            .iter()
            .all(|(_, listing)| listing.pending_buy_quantity == 0));

        let (leftover_cost, leftover_revenue) =
            value_leftovers(&crafted_items.leftovers, &tp_listings_map);
        let mut score =
            (listing_profit + leftover_revenue - leftover_cost).to_copper_value() as f64;
        if let Some(gold) = opt.min_per_hour {
            let seconds = crafted_items.crafting_seconds(item_id, &game_data.recipes)
                + market_seconds(
                    &tp_purchases,
                    vendor_purchases.len(),
                    &buy_order_quantities,
                    crafting_count,
                    opt.value.is_some(),
                    &output(&crafted_items),
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
        return Err(if crafting_count == 0 {
            stop
        } else {
            "Not profitable after charging leftovers or --min-per-hour".to_string()
        });
    }
    if best_count < crafting_count {
        if let Some(purchased_ingredients) = &mut purchased_ingredients {
            purchased_ingredients.clear();
        }
        return calculate_crafting_profit(
            item_id,
            game_data,
            api_listings_map,
            purchased_ingredients,
            known_recipes,
            &config::CraftingOptions {
                count: Some(best_count),
                min_per_hour: None,
                ..opt.clone()
            },
        );
    }

    // each unknown recipe is learned once, from its cheapest unlock item, bought or crafted after
    // the rest so it doesn't change how many are worth making
    let next_unlearned = |items: &crafting::CraftedItems, known_recipes: &HashSet<u32>| {
        let unknown = items.unknown_recipes(&game_data.recipes, known_recipes);
        // lowest id first, so a run always buys them in the same order
        let unlearned = unknown
            .into_iter()
            .filter(|id| !items.unlocks.contains_key(id));
        unlearned.min()
    };
    let mut unlocks_cost = Money::zero();
    while let Some(recipe_id) =
        known_recipes.and_then(|known| next_unlearned(&crafted_items, known))
    {
        let candidates = game_data
            .unlocked_by
            .get(&recipe_id)
            .map_or(&[][..], Vec::as_slice);
        // one item can unlock several recipes
        let unlocked = crafted_items.unlocks.values().flatten();
        let already_obtained = unlocked.copied().find(|(id, _)| candidates.contains(id));
        if already_obtained.is_some() {
            crafted_items.unlocks.insert(recipe_id, already_obtained);
            continue;
        }

        let mut cheapest: Option<(Money, u32, crafting::PreciseCraftingCostContext)> = None;
        for &unlock_id in candidates {
            let mut context = crafting::PreciseCraftingCostContext {
                purchases: vec![],
                items: crafted_items.clone(),
                being_crafted: vec![],
                lots: lots.clone(),
                seconds: 0.0,
            };
            let priced = crafting::calculate_precise_min_crafting_cost(
                unlock_id,
                1,
                game_data,
                &mut tp_listings_map,
                &lower_bounds,
                &mut context,
                opt,
            );
            // pricing reserves listings, but only the cheapest candidate is bought, below
            release_pending(&context.purchases, &mut tp_listings_map);
            if let Some(priced) = priced {
                let is_cheaper = |(cheapest_cost, ..): &(Money, _, _)| priced.cost < *cheapest_cost;
                if cheapest.as_ref().is_none_or(is_cheaper) {
                    cheapest = Some((priced.cost, unlock_id, context));
                }
            }
        }

        let unlock = if let Some((cost, unlock_id, context)) = cheapest {
            finalize_purchases(
                &context.purchases,
                &mut tp_listings_map,
                &mut tp_purchases,
                &mut vendor_purchases,
                purchased_ingredients.as_deref_mut(),
                item_id,
            );
            (crafted_items, lots) = (context.items, context.lots);
            listing_profit = listing_profit - cost;
            total_crafting_cost += cost;
            unlocks_cost += cost;
            Some((unlock_id, cost))
        } else {
            None
        };
        crafted_items.unlocks.insert(recipe_id, unlock);
    }
    let (unlock_share, _) = unlocks_cost.split(1, crafting_count);

    let (leftover_cost, leftover_revenue) =
        value_leftovers(&crafted_items.leftovers, &tp_listings_map);
    Ok(ProfitableItem {
        id: item_id,
        crafting_cost: total_crafting_cost + leftover_cost,
        profit: listing_profit + leftover_revenue - leftover_cost,
        count: crafting_count,
        max_sell: Money::from_copper(max_sell as i32),
        min_sell: Money::from_copper(min_sell as i32),
        breakeven: (breakeven + unlock_share).trading_post_listing_price(),
        crafting_steps: crafted_items
            .crafting_steps(&game_data.recipes)
            .to_integer(),
        crafting_seconds: crafted_items
            .crafting_seconds(item_id, &game_data.recipes)
            .round() as u32,
        market_seconds: market_seconds(
            &tp_purchases,
            vendor_purchases.len(),
            &buy_order_quantities,
            crafting_count,
            opt.value.is_some(),
            &output(&crafted_items),
        ),
        crafted_items,
    })
}

/// Drop the pending quantities `purchases` hold on the listings
fn release_pending(
    purchases: &[(u32, u32, crafting::Source)],
    tp_listings_map: &mut BTreeMap<u32, ItemListings>,
) {
    for &(id, count, source) in purchases {
        if source == crafting::Source::TradingPost {
            let listing = tp_listings_map.get_mut(&id);
            listing
                .unwrap_or_else(|| panic!("Missing listings for ingredient {}", id))
                .pending_buy_quantity -= count;
        }
    }
}

/// Buy `purchases` from the listings, adding them to the shopping list
fn finalize_purchases(
    purchases: &[(u32, u32, crafting::Source)],
    tp_listings_map: &mut BTreeMap<u32, ItemListings>,
    tp_purchases: &mut HashMap<u32, u32>,
    vendor_purchases: &mut HashSet<u32>,
    mut purchased_ingredients: Option<
        &mut HashMap<(u32, crafting::Source), crafting::PurchasedIngredient>,
    >,
    item_id: u32,
) {
    for (purchase_id, count, purchase_source) in purchases {
        let (cost, min_sell, max_sell) = if let crafting::Source::TradingPost = *purchase_source {
            let listing = tp_listings_map.get_mut(purchase_id).unwrap_or_else(|| {
                panic!(
                    "Missing listings for ingredient {} of item id {}",
                    purchase_id, item_id
                )
            });
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
}

/// Cost of the leftovers, and their revenue sold into buy orders, at most their cost; nothing if
/// there are none, e.g. account bound items
fn value_leftovers(
    leftovers: &HashMap<u32, (u32, Money, crafting::Source)>,
    tp_listings_map: &BTreeMap<u32, ItemListings>,
) -> (Money, Money) {
    let mut cost = Money::zero();
    let mut revenue = Money::zero();
    for (id, &(count, leftover_cost, _)) in leftovers {
        cost += leftover_cost;
        if let Some(listings) = tp_listings_map.get(id) {
            revenue += listings.sale_revenue(count).min(leftover_cost);
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
