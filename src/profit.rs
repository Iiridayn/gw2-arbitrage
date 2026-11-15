use rayon::prelude::*;

use num_traits::Zero;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::api;
use crate::config;
use crate::crafting;
use crate::item::ITEM_STACK_SIZE;
use crate::money::Money;
use crate::recipe::Recipe;
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
    api_listings: &HashMap<u32, api::ItemListings>,
    profitable_item_ids: &Vec<u32>,
    request_listing_item_ids: &[u32],
    game_data: &crafting::GameData,
    known_recipes: &HashSet<u32>,
) -> Vec<ProfitableItem> {
    let mut profitable_items: Vec<_> = profitable_item_ids
        .par_iter()
        .filter_map(|item_id| {
            let mut api_listings_for_item: HashMap<u32, _> = HashMap::new();
            for id in game_data.collect_tree_ids(&[*item_id], Some(known_recipes)) {
                debug_assert!(request_listing_item_ids.contains(&id));
                if let Some(listing) = api_listings.get(&id).cloned() {
                    api_listings_for_item.insert(id, listing);
                }
            }

            calculate_capped_profit(
                &[*item_id],
                game_data,
                &api_listings_for_item,
                Some(known_recipes),
                &CONFIG.crafting,
            )
            .ok()
            .filter(is_worth_crafting)
        })
        .collect();

    profitable_items.sort_unstable_by_key(|item| item.sales[0].id);
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
    item_ids: &[u32],
    game_data: &crafting::GameData,
    known_recipes: &HashSet<u32>,
    notify: Option<&dyn Fn(&str)>,
) -> Result<
    (
        Result<ProfitableItem, String>,
        HashMap<u32, api::ItemListings>,
    ),
    Box<dyn std::error::Error>,
> {
    let request_listing_item_ids = game_data.collect_tree_ids(item_ids, Some(known_recipes));

    let tp_listings =
        request::fetch_item_listings(&request_listing_item_ids, Some(&CONFIG.cache_dir), notify)
            .await?;
    let api_listings = vec_to_map(tp_listings, |x| x.id);

    let profitable_item = calculate_capped_profit(
        item_ids,
        game_data,
        &api_listings,
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

    Ok((profitable_item, api_listings))
}

/// A plan's score: its profit in copper, less the time it takes if --min-per-hour
fn plan_score(profit: Money, seconds: impl FnOnce() -> f64, opt: &config::CraftingOptions) -> f64 {
    let profit = profit.to_copper_value() as f64;
    match opt.min_per_hour {
        Some(gold) => profit - gold as f64 * 10000.0 * seconds() / 3600.0,
        None => profit,
    }
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
    item_ids: &[u32],
    game_data: &crafting::GameData,
    api_listings: &HashMap<u32, api::ItemListings>,
    known_recipes: Option<&HashSet<u32>>,
    opt: &config::CraftingOptions,
) -> Result<ProfitableItem, String> {
    let listings: BTreeMap<u32, ItemListings> = api_listings
        .iter()
        .map(|(&id, item_listings)| (id, ItemListings::from(item_listings.clone())))
        .collect();

    let plan = |opt: &config::CraftingOptions| -> Result<(ProfitableItem, f64), String> {
        let item =
            calculate_crafting_profit(item_ids, game_data, api_listings, known_recipes, opt)?;
        let seconds = || (item.crafting_seconds + item.market_seconds) as f64;
        let score = plan_score(item.profit, seconds, opt);
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
            if let Some((unlock_id, unlock_cost)) = item.crafted_items.unlock(recipe) {
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
    Ok(best)
}

/// Craft `item_ids` together from one market, each unit made of whichever item profits most from
/// one more, until none does
pub fn calculate_crafting_profit(
    item_ids: &[u32],
    game_data: &crafting::GameData,
    api_listings: &HashMap<u32, api::ItemListings>,
    known_recipes: Option<&HashSet<u32>>,
    opt: &config::CraftingOptions,
) -> Result<ProfitableItem, String> {
    let mut listings: BTreeMap<u32, ItemListings> = api_listings
        .clone()
        .into_iter()
        .map(|(id, item_listings)| (id, ItemListings::from(item_listings)))
        .collect();
    // before any are sold, as each item's sales fill its buy orders in shared transactions
    let unsold = listings.clone();

    let ids = game_data.collect_tree_ids(item_ids, known_recipes);
    let lowest_sells = listings.iter().filter_map(|(&id, item_listings)| {
        Some((
            id,
            Money::from_copper(item_listings.lowest_sell_offer(1)? as i32),
        ))
    });
    let lower_bounds = crafting::lower_bounds(lowest_sells.collect(), ids.into_iter(), game_data);
    // each item's sale transactions, count sold, and recipes and batches crafted, for market time
    let outputs = |items: &crafting::CraftedItems, sales: &[Sale]| -> Vec<_> {
        let outputs = sales.iter().map(|sale| {
            let transactions = match opt.value {
                Some(_) => sale.count.div_ceil(ITEM_STACK_SIZE),
                None => unsold.get(&sale.id).map_or(0, |item_listings| {
                    item_listings.sale_transactions(sale.count).len() as u32
                }),
            };
            let crafted = items.crafted.iter().filter(|((id, _), _)| *id == sale.id);
            let batches = crafted.map(|(&(id, index), &count)| {
                let recipe = &game_data.recipes[&id][index];
                (recipe, count / recipe.output_item_count)
            });
            (transactions, sale.count, batches.collect())
        });
        outputs.collect()
    };
    // what one more adds to the sales after fees, and the buy order it fills
    let sale_price = |listings: &BTreeMap<u32, ItemListings>, sale: &Sale| match opt.value {
        Some(price) => Some((Money::from_copper(price as i32), price)),
        None => {
            let top = listings.get(&sale.id)?.buys.last()?;
            let revenue = |count| unsold[&sale.id].sale_revenue(count);
            Some((
                revenue(sale.count + 1) - revenue(sale.count),
                top.unit_price,
            ))
        }
    };
    let min_profit = Money::from_copper(opt.min_per_item.unwrap_or(0) as i32);

    let mut total_crafting_cost = Money::zero();
    let mut crafting_count: u32 = 0;
    let mut crafted_items = crafting::CraftedItems::default();
    let mut purchases = crafting::Purchases::new();
    let mut score = 0.0;
    // each item's share of the score: (running total, its peak, count at the peak)
    let mut gains = vec![(0.0, 0.0, 0); item_ids.len()];
    let mut floors = vec![None; item_ids.len()]; // each item's crafting cost when last priced

    // breakeven is the last unit's crafting cost, the minimum to recoup it
    let mut sales: Vec<Sale> = item_ids
        .iter()
        .map(|&id| {
            let max_sell = listings.get(&id).map_or_else(
                || opt.value.unwrap_or(0),
                |item_listings| item_listings.buys.last().map_or(0, |l| l.unit_price),
            );
            Sale {
                id,
                max_sell: Money::from_copper(max_sell as i32),
                ..Default::default()
            }
        })
        .collect();

    // simulate crafting 1 item per loop iteration, of whichever profits most, until none does;
    // the rest of a batch is taken from leftovers
    loop {
        // (profit, index, crafting cost, buy order filled, state)
        let mut best: Option<(Money, usize, Money, u32, _)> = None;
        // each item is priced again each round, as what another made may have changed its costs
        for (index, &item_id) in item_ids.iter().enumerate() {
            let made = sales[index].count;
            if opt.count.is_some_and(|count| made >= count) {
                sales[index].stop = "Reached --count".to_string();
                continue;
            }
            if opt.item_caps.get(&item_id).is_some_and(|&cap| made >= cap) {
                sales[index].stop = "More don't pay after leftovers or --min-per-hour".to_string();
                continue;
            }
            if !game_data.recipes.contains_key(&item_id) {
                sales[index].stop = "No usable recipe; check -m, -u and the blacklists".to_string();
                continue;
            }
            let Some((buy_price, min_buy)) = sale_price(&listings, &sales[index]) else {
                sales[index].stop =
                    "No buy orders on the trading post; pass --value to price it".to_string();
                continue;
            };
            let unpaid = |cost: Money| {
                let why = if buy_price < cost {
                    "more than it sells for"
                } else {
                    "too close to its sale price for --min-per-item"
                };
                let stop = format!("Crafting costs {} each, {} ({} each)", cost, why, buy_price);
                (buy_price < cost + min_profit).then_some(stop)
            };
            let wins = |cost: Money| {
                best.as_ref()
                    .is_none_or(|&(best_profit, ..)| buy_price - cost > best_profit)
            };
            // costs only rise, so one that can't pay or win at its last cost isn't priced again
            if let Some(stop) = floors[index].and_then(unpaid) {
                sales[index].stop = stop;
                continue;
            }
            if floors[index].is_some_and(|floor| !wins(floor)) {
                continue;
            }

            // only the most profitable item is made, below
            let (priced, context) = crafting::price_one_more(
                item_id,
                game_data,
                &mut listings,
                &lower_bounds,
                &crafted_items,
                opt,
            );
            let crafting_cost = match priced {
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
                    floors[index] = Some(crafting);
                    sales[index].stop = format!(
                        "Cheaper to buy {} ({} each) than to craft ({} each)",
                        if source == crafting::Source::Vendor {
                            "from a vendor"
                        } else {
                            "on the TP"
                        },
                        cost,
                        crafting
                    );
                    continue;
                }
                _ => {
                    sales[index].stop = "An ingredient can't be bought or crafted".to_string();
                    continue;
                }
            };
            floors[index] = Some(crafting_cost);
            if let Some(stop) = unpaid(crafting_cost) {
                sales[index].stop = stop;
                continue;
            }

            if wins(crafting_cost) {
                best = Some((
                    buy_price - crafting_cost,
                    index,
                    crafting_cost,
                    min_buy,
                    context,
                ));
            }
        }
        let Some((profit, index, crafting_cost, min_buy, context)) = best else {
            break;
        };

        let item_id = item_ids[index];
        if opt.value.is_none() {
            listings.get_mut(&item_id).unwrap().sell(1);
        }
        total_crafting_cost += crafting_cost;
        crafting_count += 1;
        let sale = &mut sales[index];
        sale.count += 1;
        sale.profit += profit;
        (sale.min_sell, sale.breakeven) = (Money::from_copper(min_buy as i32), crafting_cost);
        crafted_items = context.items;

        finalize_purchases(&context.purchases, &mut listings, &mut purchases, item_id);
        debug_assert!(listings
            .iter()
            .all(|(_, listing)| listing.pending_buy_quantity == 0));

        let (leftover_cost, leftover_revenue) =
            value_leftovers(&crafted_items.leftovers, &listings);
        let sales_profit: Money = sales.iter().map(|sale| sale.profit).sum();
        let seconds = || {
            crafted_items.crafting_seconds(item_ids, &game_data.recipes)
                + market_seconds(&purchases, &outputs(&crafted_items, &sales)) as f64
        };
        let new_score = plan_score(
            sales_profit + leftover_revenue - leftover_cost,
            seconds,
            opt,
        );
        let made = sales[index].count;
        let (total, peak, peak_count) = &mut gains[index];
        *total += new_score - score;
        score = new_score;
        if *total > *peak {
            (*peak, *peak_count) = (*total, made);
        }
    }

    // Stop each item where its share of the score peaked, after selling leftovers and, with
    // --min-per-hour, the time spent
    let peaks: Vec<u32> = gains.iter().map(|&(.., count)| count).collect();
    if peaks.iter().all(|&count| count == 0) {
        return Err(if crafting_count > 0 {
            "Not profitable after charging leftovers or --min-per-hour".to_string()
        } else if let [sale] = &sales[..] {
            sale.stop.clone()
        } else {
            let stop = |sale: &Sale| format!("{}: {}", game_data.item_name(sale.id), sale.stop);
            sales.iter().map(stop).collect::<Vec<_>>().join("\n")
        });
    }
    // capped items stop competing for listings, so the rest are planned again; caps only fall, so
    // this ends
    if sales
        .iter()
        .zip(&peaks)
        .any(|(sale, &peak)| peak < sale.count)
    {
        // only items past their peak; the rest, never made ones included, are priced again
        let mut item_caps = opt.item_caps.clone();
        for (sale, peak) in sales.iter().zip(peaks) {
            if peak < sale.count {
                item_caps.insert(sale.id, peak);
            }
        }
        return calculate_crafting_profit(
            item_ids,
            game_data,
            api_listings,
            known_recipes,
            &config::CraftingOptions {
                item_caps,
                ..opt.clone()
            },
        );
    }

    if let Some(known_recipes) = known_recipes {
        total_crafting_cost += buy_unlocks(
            known_recipes,
            game_data,
            &mut listings,
            &lower_bounds,
            &mut crafted_items,
            &mut purchases,
            opt,
        );
    }

    let (leftover_cost, leftover_revenue) = value_leftovers(&crafted_items.leftovers, &listings);
    let market_seconds = market_seconds(&purchases, &outputs(&crafted_items, &sales));

    charge_unlocks(&mut sales, &crafted_items, &game_data.recipes);
    let profit = sales.iter().map(|sale| sale.profit).sum::<Money>() + leftover_revenue;
    for sale in sales.iter_mut().filter(|sale| sale.count > 0) {
        sale.breakeven = sale.breakeven.trading_post_listing_price();
        sale.stop.clear();
    }
    Ok(ProfitableItem {
        sales,
        crafting_cost: total_crafting_cost + leftover_cost,
        profit: profit - leftover_cost,
        crafting_steps: crafted_items
            .crafting_steps(&game_data.recipes)
            .to_integer(),
        crafting_seconds: crafted_items
            .crafting_seconds(item_ids, &game_data.recipes)
            .round() as u32,
        market_seconds,
        crafted_items,
        purchases,
    })
}

/// Learn each unknown recipe once, from its cheapest unlock item, bought or crafted after the rest
/// so it doesn't change how many are worth making; returns what the unlock items cost
fn buy_unlocks(
    known_recipes: &HashSet<u32>,
    game_data: &crafting::GameData,
    listings: &mut BTreeMap<u32, ItemListings>,
    lower_bounds: &HashMap<u32, Money>,
    crafted_items: &mut crafting::CraftedItems,
    purchases: &mut crafting::Purchases,
    opt: &config::CraftingOptions,
) -> Money {
    let next_unlearned = |items: &crafting::CraftedItems| {
        let unknown = items.unknown_recipes(&game_data.recipes, known_recipes);
        // lowest id first, so a run always buys them in the same order
        let unlearned = unknown
            .into_iter()
            .filter(|id| !items.unlocks.contains_key(id));
        unlearned.min()
    };
    let mut total_cost = Money::zero();
    while let Some(recipe_id) = next_unlearned(crafted_items) {
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
            // only the cheapest candidate is bought, below
            let (priced, context) = crafting::price_one_more(
                unlock_id,
                game_data,
                listings,
                lower_bounds,
                crafted_items,
                opt,
            );
            if let Some(priced) = priced {
                let is_cheaper = |(cheapest_cost, ..): &(Money, _, _)| priced.cost < *cheapest_cost;
                if cheapest.as_ref().is_none_or(is_cheaper) {
                    cheapest = Some((priced.cost, unlock_id, context));
                }
            }
        }

        let unlock = if let Some((cost, unlock_id, context)) = cheapest {
            finalize_purchases(&context.purchases, listings, purchases, unlock_id);
            *crafted_items = context.items;
            total_cost += cost;
            Some((unlock_id, cost))
        } else {
            None
        };
        crafted_items.unlocks.insert(recipe_id, unlock);
    }
    total_cost
}

/// Split each unlock item's cost by count between the items made whose steps learn from it
fn charge_unlocks(
    sales: &mut [Sale],
    crafted_items: &crafting::CraftedItems,
    recipes: &HashMap<u32, Vec<Recipe>>,
) {
    let learns: Vec<HashSet<u32>> = sales
        .iter()
        .map(|sale| {
            let steps = crafted_items.sorted(&[sale.id], recipes);
            let unlocks = steps
                .iter()
                .filter_map(|(.., recipe)| crafted_items.unlock(recipe));
            unlocks.map(|(unlock_id, _)| unlock_id).collect()
        })
        .collect();
    let bought: BTreeMap<u32, Money> = crafted_items.unlocks.values().flatten().copied().collect();
    for (unlock_id, mut cost) in bought {
        let users =
            (0..sales.len()).filter(|&i| sales[i].count > 0 && learns[i].contains(&unlock_id));
        let users: Vec<usize> = users.collect();
        let mut units: u32 = users.iter().map(|&i| sales[i].count).sum();
        debug_assert!(
            units > 0,
            "No item made learns from unlock item {}",
            unlock_id
        );
        for i in users {
            let sale = &mut sales[i];
            let (share, rest) = cost.split(sale.count, units);
            (cost, units) = (rest, units - sale.count);
            sale.profit = sale.profit - share;
            sale.breakeven += share / sale.count;
        }
    }
}

/// Buy `pending` from the listings, adding them to the shopping list
fn finalize_purchases(
    pending: &[(u32, u32, crafting::Source)],
    listings: &mut BTreeMap<u32, ItemListings>,
    purchases: &mut crafting::Purchases,
    item_id: u32,
) {
    for &(purchase_id, count, purchase_source) in pending {
        let (cost, min_sell, max_sell) = if let crafting::Source::TradingPost = purchase_source {
            let listing = listings.get_mut(&purchase_id).unwrap_or_else(|| {
                panic!(
                    "Missing listings for ingredient {} of item id {}",
                    purchase_id, item_id
                )
            });
            listing.buy(count).unwrap_or_else(|| {
                panic!(
                    "Expected to be able to buy {} of ingredient {} for item id {}",
                    count, purchase_id, item_id
                )
            })
        } else {
            (0, 0, 0)
        };

        let ingredient = purchases.entry((purchase_id, purchase_source)).or_default();
        ingredient.count += count;
        if ingredient.min_price.is_zero() {
            ingredient.min_price = Money::from_copper(min_sell as i32);
        }
        ingredient.max_price = Money::from_copper(max_sell as i32);
        ingredient.total_cost += Money::from_copper(cost as i32);
    }
}

/// Cost of the leftovers, and their revenue sold into buy orders, at most their cost; nothing if
/// there are none, e.g. account bound items
fn value_leftovers(
    leftovers: &HashMap<u32, (u32, Money, crafting::Source)>,
    listings: &BTreeMap<u32, ItemListings>,
) -> (Money, Money) {
    let mut cost = Money::zero();
    let mut revenue = Money::zero();
    for (id, &(count, leftover_cost, _)) in leftovers {
        cost += leftover_cost;
        if let Some(item_listings) = listings.get(id) {
            revenue += item_listings.sale_revenue(count).min(leftover_cost);
        }
    }
    (cost, revenue)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sale {
    pub id: u32,
    pub count: u32,
    pub max_sell: Money,
    pub min_sell: Money,
    pub breakeven: Money,
    pub profit: Money, // its sales after fees, less its units' costs and its share of unlock items
    pub stop: String,  // why none were made
}

#[derive(Debug, Eq, PartialEq)]
pub struct ProfitableItem {
    pub sales: Vec<Sale>, // each item asked for, made or not, in the order asked for
    pub crafting_cost: Money,
    pub profit: Money,
    pub crafting_steps: u32,
    pub crafting_seconds: u32,
    pub market_seconds: u32,
    pub crafted_items: crafting::CraftedItems,
    pub purchases: crafting::Purchases,
}

impl ProfitableItem {
    pub fn count(&self) -> u32 {
        self.sales.iter().map(|sale| sale.count).sum()
    }

    pub fn profit_per_item(&self) -> Money {
        self.profit / self.count()
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
