use crate::api;
use crate::config;
use crate::item::Item;
use crate::money::Money;
use crate::recipe::{self, Discipline, Recipe};
use crate::trading_post;

use num_rational::Ratio;
use num_traits::Zero;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Logging out and in to use another character's crafting disciplines
const CHARACTER_SWITCH_SECONDS: f64 = 30.0;

#[derive(Debug, Copy, Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Source {
    Crafting,
    TradingPost,
    Vendor,
}

#[derive(Debug, Copy, Clone)]
pub struct EstimatedCraftingCost {
    pub cost: Money,
    pub source: Source,
}

pub struct GameData {
    pub recipes: HashMap<u32, Vec<Recipe>>, // output item id, its recipes
    pub items: HashMap<u32, Item>,
}

impl GameData {
    /// Recipes are kept in the order given, less any that make the same as an earlier one
    pub fn new(recipes: impl IntoIterator<Item = Recipe>, items: Vec<Item>) -> Self {
        let mut recipes_map: HashMap<u32, Vec<Recipe>> = HashMap::new();
        for recipe in recipes {
            let kept = recipes_map.entry(recipe.output_item_id).or_default();
            if !kept.iter().any(|other| other.is_same_as(&recipe)) {
                kept.push(recipe);
            }
        }
        GameData {
            recipes: recipes_map,
            items: items.into_iter().map(|item| (item.id, item)).collect(),
        }
    }
}

pub struct SummaryPrices {
    pub tp_prices: HashMap<u32, api::Price>,
    pub lower_bounds: HashMap<u32, Money>, // id, the least the estimate can price one at
}

impl SummaryPrices {
    pub fn new(tp_prices: HashMap<u32, api::Price>, game_data: &GameData) -> Self {
        let lowest_sells = tp_prices
            .values()
            .filter(|price| price.sells.quantity > 0)
            .map(|price| (price.id, Money::from_copper(price.sells.unit_price as i32)))
            .collect();
        let (items, recipes) = (&game_data.items, game_data.recipes.keys());
        let ids = items
            .keys()
            .chain(recipes.filter(|id| !items.contains_key(id)));
        let lower_bounds = lower_bounds(lowest_sells, ids.copied(), game_data);
        SummaryPrices {
            tp_prices,
            lower_bounds,
        }
    }
}

/// The least one of each of `ids` can cost, from their lowest TP sells, vendor and token lots, and
/// recipes; ingredients of their recipes must be in `ids`
pub fn lower_bounds(
    lowest_sells: HashMap<u32, Money>,
    ids: impl Iterator<Item = u32> + Clone,
    game_data: &GameData,
) -> HashMap<u32, Money> {
    let mut lower_bounds = lowest_sells;
    // cost / n, rounded down: split's smallest share
    let per_item = |cost: Money, n: u32| cost.split(n - 1, n).1;
    for item in ids.clone().filter_map(|id| game_data.items.get(&id)) {
        if let Some((lot_cost, lot_size)) = item.vendor_cost().or_else(|| item.token_value()) {
            let bound = per_item(lot_cost, lot_size);
            let entry = lower_bounds.entry(item.id).or_insert(bound);
            *entry = (*entry).min(bound);
        }
    }
    loop {
        let mut lowered = false;
        for (&id, recipes) in ids
            .clone()
            .filter_map(|id| game_data.recipes.get_key_value(&id))
        {
            for recipe in recipes {
                let batch = recipe.output_item_count;
                let bound = recipe_bound(recipe, batch, &lower_bounds)
                    .map(|cost| per_item(cost, batch))
                    .filter(|&bound| lower_bounds.get(&id).is_none_or(|&current| bound < current));
                if let Some(bound) = bound {
                    lower_bounds.insert(id, bound);
                    lowered = true;
                }
            }
        }
        if !lowered {
            return lower_bounds;
        }
    }
}

/// The least crafting `count` with `recipe` can cost, if all its ingredients have a bound
fn recipe_bound(recipe: &Recipe, count: u32, lower_bounds: &HashMap<u32, Money>) -> Option<Money> {
    let batches = count.div_ceil(recipe.output_item_count);
    let mut ingredients = recipe.ingredients.iter();
    let batch = ingredients.try_fold(recipe.currency_cost()?, |cost, ingredient| {
        Some(cost + *lower_bounds.get(&ingredient.item_id)? * ingredient.count)
    })?;
    Some(
        (batch * batches)
            .split(count, batches * recipe.output_item_count)
            .0,
    )
}

/// Recipe id to its cheapest unlock item id and estimated cost, or None if it has none with a price
pub type RequiredUnlocks = BTreeMap<u32, Option<(u32, Money)>>;

/// Items that unlock recipes, for costing recipes the account doesn't know
pub struct RecipeUnlocks<'a> {
    pub known_recipes: &'a HashSet<u32>,
    // recipe id to unlock item ids
    unlock_items: HashMap<u32, Vec<u32>>,
    // recipes being costed, so an unlock chain can't loop back to itself
    visiting: RefCell<HashSet<u32>>,
}

impl<'a> RecipeUnlocks<'a> {
    pub fn new(items_map: &HashMap<u32, Item>, known_recipes: &'a HashSet<u32>) -> Self {
        let mut unlock_items: HashMap<u32, Vec<u32>> = HashMap::new();
        for item in items_map.values() {
            for recipe_id in item.recipe_unlocks().into_iter().flatten() {
                unlock_items.entry(recipe_id).or_default().push(item.id);
            }
        }
        RecipeUnlocks {
            known_recipes,
            unlock_items,
            visiting: Default::default(),
        }
    }

    /// Unlock items needed for the recipes used in crafting
    pub fn required(
        &self,
        crafted_items: &CraftedItems,
        game_data: &GameData,
        prices: &SummaryPrices,
        opt: &config::CraftingOptions,
    ) -> RequiredUnlocks {
        crafted_items
            .unknown_recipes(&game_data.recipes, self.known_recipes)
            .into_iter()
            .map(|recipe_id| {
                let unlock = self.cheapest_unlock(recipe_id, game_data, prices, opt);
                (recipe_id, unlock)
            })
            .collect()
    }

    /// Cost of a recipe's cheapest unlock item; zero if the recipe is known
    fn cost(
        &self,
        recipe: &Recipe,
        game_data: &GameData,
        prices: &SummaryPrices,
        opt: &config::CraftingOptions,
    ) -> Option<Money> {
        match recipe.id {
            Some(id) if !recipe.is_automatic() && !self.known_recipes.contains(&id) => self
                .cheapest_unlock(id, game_data, prices, opt)
                .map(|(_, cost)| cost),
            _ => Some(Money::zero()),
        }
    }

    fn cheapest_unlock(
        &self,
        recipe_id: u32,
        game_data: &GameData,
        prices: &SummaryPrices,
        opt: &config::CraftingOptions,
    ) -> Option<(u32, Money)> {
        if !self.visiting.borrow_mut().insert(recipe_id) {
            return None;
        }
        let cheapest = self
            .unlock_items
            .get(&recipe_id)
            .into_iter()
            .flatten()
            .filter_map(|&item_id| {
                let cost = calculate_estimated_min_crafting_cost(
                    item_id,
                    1,
                    game_data,
                    prices,
                    Some(self),
                    &mut vec![],
                    opt,
                )?;
                Some((item_id, cost.cost))
            })
            .min_by_key(|&(_, cost)| cost);
        self.visiting.borrow_mut().remove(&recipe_id);
        cheapest
    }
}

// Calculate the lowest cost method to obtain count of the given item, using only the current high/low
// tp prices. This may involve a combination of crafting, trading and buying from vendors.
// Unlock item prices are best-effort estimated
pub fn calculate_estimated_min_crafting_cost(
    item_id: u32,
    count: u32,
    game_data: &GameData,
    prices: &SummaryPrices,
    unlocks: Option<&RecipeUnlocks>,
    being_crafted: &mut Vec<u32>,
    opt: &config::CraftingOptions,
) -> Option<EstimatedCraftingCost> {
    let item = game_data.items.get(&item_id);
    // buying is priced before crafting, so a recipe can be skipped when even its lower bound
    // can't beat buying or an earlier recipe
    let tp_cost = prices
        .tp_prices
        .get(&item_id)
        .filter(|price| price.sells.quantity > 0)
        .map(|price| Money::from_copper(price.sells.unit_price as i32) * count);

    let vendor_cost = item
        .and_then(|item| item.vendor_cost().or_else(|| item.token_value()))
        .map(|(lot_cost, lot_size)| {
            let lots = count.div_ceil(lot_size);
            let (share, _leftovers) = (lot_cost * lots).split(count, lots * lot_size);
            share
        });

    let recipes = game_data
        .recipes
        .get(&item_id)
        .filter(|_| !being_crafted.contains(&item_id));

    being_crafted.push(item_id);
    let mut price_recipe = |recipe: &Recipe| {
        if !opt.include_timegated && recipe.is_timegated() {
            None
        } else {
            let batches = count.div_ceil(recipe.output_item_count);
            let mut cost = recipe.currency_cost()? * batches;
            for ingredient in &recipe.ingredients {
                let ingredient_cost = calculate_estimated_min_crafting_cost(
                    ingredient.item_id,
                    ingredient.count * batches,
                    game_data,
                    prices,
                    unlocks,
                    being_crafted,
                    opt,
                );

                if let Some(EstimatedCraftingCost {
                    cost: ingredient_cost,
                    ..
                }) = ingredient_cost
                {
                    cost += ingredient_cost;
                } else {
                    return None;
                }
            }

            let unlock_cost = match unlocks {
                Some(unlocks) => unlocks.cost(recipe, game_data, prices, opt)?,
                None => Money::zero(),
            };
            let (share, _leftovers) = cost.split(count, batches * recipe.output_item_count);
            Some(share + unlock_cost)
        }
    };
    let mut crafting_cost = None;
    for recipe in recipes.into_iter().flatten() {
        let best = tp_cost.inner_min(vendor_cost).inner_min(crafting_cost);
        let bound = recipe_bound(recipe, count, &prices.lower_bounds);
        if bound.zip(best).is_none_or(|(bound, best)| bound < best) {
            crafting_cost = crafting_cost.inner_min(price_recipe(recipe));
        }
    }
    being_crafted.pop();

    let cost = tp_cost.inner_min(crafting_cost).inner_min(vendor_cost)?;

    // if costs are equal, prefer the trading post, then the vendor, then crafting
    let source = if tp_cost == Some(cost) {
        Source::TradingPost
    } else if vendor_cost == Some(cost) {
        Source::Vendor
    } else {
        Source::Crafting
    };

    Some(EstimatedCraftingCost { cost, source })
}

// Exact

#[derive(Debug, Copy, Clone)]
pub struct PreciseCraftingCost {
    pub cost: Money,
    pub source: Source,
    // the cost of crafting it, if possible, whichever source is cheapest
    pub crafting: Option<Money>,
}

#[derive(Clone)]
pub struct PreciseCraftingCostContext {
    pub purchases: Vec<(u32, u32, Source)>, // id, count, Source
    pub items: CraftedItems,
    pub being_crafted: Vec<u32>,          // outermost first
    pub lots: HashMap<u32, (Money, u32)>, // id, (cost, count) of its leftovers' batches or lot
    pub seconds: f64,                     // crafting, for ties between recipes
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct CraftedItems {
    pub crafted: HashMap<(u32, usize), u32>, // (id, recipe index), count
    pub leftovers: HashMap<u32, (u32, Money, Source)>, // id, (count, total cost, source)
}

impl CraftedItems {
    pub fn crafting_steps(&self, recipes_map: &HashMap<u32, Vec<Recipe>>) -> Ratio<u32> {
        let total_crafting_steps = self
            .crafted
            .iter()
            .map(|(&(item_id, index), &count)| {
                Ratio::new(count, recipes_map[&item_id][index].output_item_count)
            })
            .reduce(|total, count| total + count)
            .unwrap();
        debug_assert!(total_crafting_steps.is_integer());
        total_crafting_steps
    }

    /// Estimated seconds to craft everything, with one batch per crafted item
    pub fn crafting_seconds(&self, item_id: u32, recipes_map: &HashMap<u32, Vec<Recipe>>) -> f64 {
        let crafting: f64 = self
            .crafted
            .iter()
            .map(|(&(item_id, index), &count)| {
                let recipe = &recipes_map[&item_id][index];
                recipe.crafting_seconds(count / recipe.output_item_count)
            })
            .sum();
        let step_groups = discipline_groups(&mut self.sorted(item_id, recipes_map));
        let groups: Vec<usize> = step_groups.into_iter().flatten().collect();
        let switches = groups.windows(2).filter(|w| w[0] != w[1]).count();
        crafting + switches as f64 * CHARACTER_SWITCH_SECONDS
    }

    pub fn unknown_recipes(
        &self,
        recipes_map: &HashMap<u32, Vec<Recipe>>,
        known_recipes: &HashSet<u32>,
    ) -> HashSet<u32> {
        let mut unknown_recipes = HashSet::new();
        for &(item_id, index) in self.crafted.keys() {
            let recipe = &recipes_map[&item_id][index];
            if let Some(id) = recipe.id {
                if !recipe.is_automatic() && !known_recipes.contains(&id) {
                    unknown_recipes.insert(id);
                }
            }
        }
        unknown_recipes
    }

    /// Sort the steps making `item_ids`, by each recipe used, in the order which will remove
    /// ingredients from the inventory fastest; also returns the items consumed by these steps and
    /// the steps under them
    fn sort_ingredients<'a>(
        &self,
        item_ids: impl Iterator<Item = u32>,
        recipes_map: &'a HashMap<u32, Vec<Recipe>>,
        being_crafted: &mut Vec<u32>,
    ) -> (u32, Vec<(u32, u32, &'a Recipe)>) {
        let mut ingredients = Vec::new();
        for item_id in item_ids {
            if being_crafted.contains(&item_id) {
                continue;
            }
            for (&key, &count) in self.crafted.iter().filter(|((id, _), _)| *id == item_id) {
                // Let each recipe which uses something get full credit for it; then
                // only add it once to the crafting list once weights are determined
                ingredients.push((key, self.sort_steps(key, count, recipes_map, being_crafted)));
            }
        }

        // Put first the ingredient whose steps consume the most items, to empty the inventory
        // fastest; ties go to the higher item id, then recipe
        ingredients.sort_by(|(a_key, (a_sum, _)), (b_key, (b_sum, _))| {
            b_sum.cmp(a_sum).then(b_key.cmp(a_key))
        });

        let mut sorted: Vec<(u32, u32, &Recipe)> = Vec::new();
        let mut sum = 0;
        for (_, (steps_sum, steps)) in ingredients {
            sum += steps_sum;
            for step in steps {
                if !sorted
                    .iter()
                    .any(|s| s.0 == step.0 && std::ptr::eq(s.2, step.2))
                {
                    sorted.push(step);
                }
            }
        }
        (sum, sorted)
    }

    /// The steps under crafting `count` of an item by one of its recipes, then that step; also
    /// returns the items they consume
    fn sort_steps<'a>(
        &self,
        (item_id, index): (u32, usize),
        count: u32,
        recipes_map: &'a HashMap<u32, Vec<Recipe>>,
        being_crafted: &mut Vec<u32>,
    ) -> (u32, Vec<(u32, u32, &'a Recipe)>) {
        let recipe = &recipes_map[&item_id][index];
        being_crafted.push(item_id);
        let ingredient_ids = recipe.ingredients.iter().map(|i| i.item_id);
        let (sum, mut sorted) = self.sort_ingredients(ingredient_ids, recipes_map, being_crafted);
        being_crafted.pop();

        let consumed: u32 = recipe.ingredients.iter().map(|i| i.count).sum();
        sorted.push((item_id, count, recipe));
        (sum + consumed * count / recipe.output_item_count, sorted)
    }

    // TODO: use an iterator? - complains about lifetimes though, despite copy data output w/o references
    pub fn sorted<'a>(
        &self,
        item_id: u32,
        recipes_map: &'a HashMap<u32, Vec<Recipe>>,
    ) -> Vec<(u32, u32, &'a Recipe)> {
        self.sort_ingredients(std::iter::once(item_id), recipes_map, &mut vec![])
            .1
    }
}

/// Group crafting station steps, starting from the most restrictive step, and
/// reorder the steps to avoid switching back to a group. Returns each step's
/// group index; None for steps not at a station (salvage, double click, …).
pub fn discipline_groups(steps: &mut Vec<(u32, u32, &Recipe)>) -> Vec<Option<usize>> {
    let disciplines = |recipe: &Recipe| -> Vec<Discipline> {
        let crafting = recipe.disciplines.iter().copied();
        crafting.filter(Discipline::is_station).collect()
    };
    let mut order: Vec<usize> = (0..steps.len()).collect();
    order.sort_by_key(|&i| disciplines(steps[i].2).len());

    let mut groups: Vec<Vec<Discipline>> = vec![];
    let mut step_groups = vec![None; steps.len()];
    for i in order {
        let own = disciplines(steps[i].2);
        if own.is_empty() {
            continue;
        }
        let shared = groups
            .iter()
            .position(|group| group.iter().any(|d| own.contains(d)));
        let group = shared.unwrap_or_else(|| {
            groups.push(own.clone());
            groups.len() - 1
        });
        groups[group].retain(|d| own.contains(d));
        step_groups[i] = Some(group);
    }

    // Working back from the last step, take the latest step whose output is no
    // longer needed, preferring the current group
    let mut remaining: Vec<usize> = (0..steps.len()).collect();
    let mut order = vec![];
    let mut current = None;
    while !remaining.is_empty() {
        let used = |i: usize| {
            let output = steps[i].0;
            let uses =
                |&j: &usize| j != i && steps[j].2.ingredients.iter().any(|g| g.item_id == output);
            remaining.iter().any(uses)
        };
        let ready = || remaining.iter().rev().copied().filter(|&i| !used(i));
        // items in a loop use each other's outputs, so none is ready; take the latest step
        let pick = ready()
            .find(|&i| current.is_none() || step_groups[i].is_none() || step_groups[i] == current)
            .or_else(|| ready().next())
            .unwrap_or(*remaining.last().unwrap());
        current = step_groups[pick].or(current);
        remaining.retain(|&i| i != pick);
        order.push(pick);
    }
    order.reverse();
    *steps = order.iter().map(|&i| steps[i]).collect();
    order.iter().map(|&i| step_groups[i]).collect()
}

/// Each group's shared crafting disciplines and highest level, e.g. "Am/L/T 400"
pub fn disciplines_labels(
    steps: &[(u32, u32, &Recipe)],
    step_groups: &[Option<usize>],
) -> Vec<String> {
    let count = step_groups
        .iter()
        .flatten()
        .max()
        .map_or(0, |group| group + 1);
    (0..count)
        .map(|group| {
            let recipes: Vec<&Recipe> = steps
                .iter()
                .zip(step_groups)
                .filter(|(_, step_group)| **step_group == Some(group))
                .map(|(step, _)| step.2)
                .collect();
            let mut shared = recipes[0].disciplines.clone();
            shared.retain(|d| d.is_station() && recipes.iter().all(|r| r.disciplines.contains(d)));
            let abbrevs: Vec<String> = shared.iter().map(Discipline::get_abbrev).collect();
            match recipes.iter().filter_map(|r| r.min_rating).max() {
                Some(rating) => format!("{} {}", abbrevs.join("/"), rating),
                None => abbrevs.join("/"),
            }
        })
        .collect()
}

// Calculate the lowest cost method to obtain the given item, with simulated purchases from
// the trading post. Each TP price level is priced separately, so cheap listings can be bought
// and the rest crafted. The source is Crafting only if every level was crafted.
pub fn calculate_precise_min_crafting_cost(
    item_id: u32,
    item_count: u32,
    game_data: &GameData,
    tp_listings_map: &mut BTreeMap<u32, trading_post::ItemListings>,
    lower_bounds: &HashMap<u32, Money>,
    context: &mut PreciseCraftingCostContext,
    opt: &config::CraftingOptions,
) -> Option<PreciseCraftingCost> {
    let mut total = PreciseCraftingCost {
        cost: Money::zero(),
        source: Source::Crafting,
        crafting: Some(Money::zero()),
    };
    let mut remaining = item_count;
    while remaining > 0 {
        let level = tp_listings_map
            .get(&item_id)
            .and_then(|listings| listings.lowest_price_quantity());
        let count = level.map_or(remaining, |quantity| quantity.min(remaining));
        let chunk = price_from_one_source(
            item_id,
            count,
            game_data,
            tp_listings_map,
            lower_bounds,
            context,
            opt,
        )?;
        total.cost += chunk.cost;
        total.crafting = total.crafting.zip(chunk.crafting).map(|(a, b)| a + b);
        if chunk.source != Source::Crafting {
            total.source = chunk.source;
        }
        remaining -= count;
    }
    Some(total)
}

// Price all of item_count from the cheapest single source
fn price_from_one_source(
    item_id: u32,
    item_count: u32,
    game_data: &GameData,
    tp_listings_map: &mut BTreeMap<u32, trading_post::ItemListings>,
    lower_bounds: &HashMap<u32, Money>,
    context: &mut PreciseCraftingCostContext,
    opt: &config::CraftingOptions,
) -> Option<PreciseCraftingCost> {
    let item = game_data.items.get(&item_id);
    let recipes = game_data
        .recipes
        .get(&item_id)
        .filter(|_| !context.being_crafted.contains(&item_id));

    // Take from leftovers first if any
    let (item_count, cost_of_leftovers_used) =
        if let Some((count, cost, source)) = context.items.leftovers.remove(&item_id) {
            match count.cmp(&item_count) {
                std::cmp::Ordering::Less => {
                    // Source is only checked against crafting to break out of profit loop; so prefer
                    // whichever other source
                    (item_count - count, cost)
                }
                std::cmp::Ordering::Equal => {
                    return Some(PreciseCraftingCost {
                        cost,
                        source,
                        crafting: None,
                    })
                }
                std::cmp::Ordering::Greater => {
                    // split the lot by all of it used so far, so the shares don't depend on how
                    // many were taken at a time
                    let (lot_cost, lot_count) = context.lots[&item_id];
                    let used = lot_count - count + item_count;
                    let (_, rest) = lot_cost.split(used, lot_count);
                    let taken = cost - rest;
                    context
                        .items
                        .leftovers
                        .insert(item_id, (count - item_count, rest, source));
                    return Some(PreciseCraftingCost {
                        cost: taken,
                        source,
                        crafting: None,
                    });
                }
            }
        } else {
            (item_count, Money::zero())
        };

    // buying is priced first, so a recipe whose lower bound can't beat buying is skipped; never for
    // the item being priced itself, whose crafting cost is shown even when buying is cheaper
    let tp_cost = tp_listings_map
        .get(&item_id)
        .and_then(|listings| listings.lowest_sell_offer(item_count))
        .map(|offer| Money::from_copper(offer as i32));

    let (vendor_cost, vendor_purchase) = item
        .and_then(|item| item.vendor_cost().or_else(|| item.token_value()))
        .map(|(lot_cost, lot_size)| {
            let lots = item_count.div_ceil(lot_size);
            let purchase_cost = lot_cost * lots;
            let (share, rest) = purchase_cost.split(item_count, lots * lot_size);
            (share, (lots * lot_size, purchase_cost, rest))
        })
        .unzip();

    // what crafting must beat
    let buying = tp_cost
        .inner_min(vendor_cost)
        .filter(|_| !context.being_crafted.is_empty());
    // numbered as in `crafted`, then tried cheapest-looking first
    let mut tried: Vec<_> = recipes.into_iter().flatten().enumerate().collect();
    let bounds: Vec<_> = tried
        .iter()
        .map(|(_, recipe)| recipe_bound(recipe, item_count, lower_bounds))
        .collect();
    tried.sort_by_key(|&(index, _)| bounds[index]);

    // the start every recipe is priced from
    let purchases_ptr = context.purchases.len();
    let start = context.clone();
    // (share, seconds, index), batches' cost, state
    let mut best: Option<((Money, f64, usize), Money, PreciseCraftingCostContext)> = None;
    context.being_crafted.push(item_id);
    for (index, recipe) in tried {
        // skip what can't win; an equal bound is still tried, so time can break the tie
        let skip = bounds[index].is_some_and(|bound| {
            buying.is_some_and(|buying| bound >= buying)
                || best
                    .as_ref()
                    .is_some_and(|((share, ..), ..)| bound > *share)
        });
        if skip {
            continue;
        }

        // whole batches; pricing them buys ingredients and crafts sub-items, changing the state
        let batches = item_count.div_ceil(recipe.output_item_count);
        let output_count = batches * recipe.output_item_count;
        let cost = recipe.currency_cost();
        let cost = cost.filter(|_| opt.include_timegated || !recipe.is_timegated());
        let cost = cost.and_then(|currencies| {
            let mut ingredients = recipe.ingredients.iter();
            ingredients.try_fold(currencies * batches, |cost, ingredient| {
                let count = ingredient.count * batches;
                let precise = calculate_precise_min_crafting_cost(
                    ingredient.item_id,
                    count,
                    game_data,
                    tp_listings_map,
                    lower_bounds,
                    context,
                    opt,
                );
                Some(cost + precise?.cost)
            })
        });

        // its crafting time: only the extra batches if it's already in the plan
        let done = context.items.crafted.get(&(item_id, index));
        let done = done.map(|count| count / recipe.output_item_count);
        let spent = done.map_or(0.0, |done| recipe.crafting_seconds(done));
        context.seconds += recipe.crafting_seconds(done.unwrap_or(0) + batches) - spent;
        // and each thing not already on the shopping list must be found and bought
        let (before, bought) = context.purchases.split_at(purchases_ptr);
        let shopping = bought.iter().enumerate().filter(|&(i, &(id, _, source))| {
            let same = |&(other, _, other_source): &(u32, u32, Source)| {
                (other, other_source) == (id, source)
            };
            !before.iter().any(same) && !bought[..i].iter().any(same)
        });
        let shopping = shopping.count() as f64 * recipe::INGREDIENT_SWITCH_SECONDS;
        let seconds = context.seconds - start.seconds + shopping;

        // back to the start, keeping this attempt if it beats the best on (share, seconds, index)
        let attempt = restore(context, tp_listings_map, purchases_ptr, start.clone());
        if let Some(cost) = cost {
            let key = (cost.split(item_count, output_count).0, seconds, index);
            if best.as_ref().is_none_or(|best| key < best.0) {
                best = Some((key, cost, attempt));
            }
        }
    }
    context.being_crafted.pop();
    let crafting_cost = best.as_ref().map(|((share, ..), ..)| *share);
    let cost = tp_cost.inner_min(crafting_cost).inner_min(vendor_cost)?;

    // if costs are equal, prefer the trading post, then the vendor, then crafting
    let source = if tp_cost == Some(cost) {
        Source::TradingPost
    } else if vendor_cost == Some(cost) {
        Source::Vendor
    } else {
        Source::Crafting
    };

    if source == Source::Crafting {
        // put the winner's state back
        let ((_, _, index), batches_cost, attempt) = best.unwrap();
        restore(context, tp_listings_map, purchases_ptr, attempt);
        let output_item_count = recipes.unwrap()[index].output_item_count;
        let output_count = item_count.div_ceil(output_item_count) * output_item_count;
        *context.items.crafted.entry((item_id, index)).or_insert(0) += output_count;
        if output_count > item_count {
            // Should never have leftovers if we're crafting more
            debug_assert!(!context.items.leftovers.contains_key(&item_id));
            context.items.leftovers.insert(
                item_id,
                (
                    output_count - item_count,
                    batches_cost.split(item_count, output_count).1,
                    Source::Crafting,
                ),
            );
            context.lots.insert(item_id, (batches_cost, output_count));
        }
    }

    // Mark for purchase
    if source == Source::TradingPost {
        context.purchases.push((item_id, item_count, source));
        tp_listings_map
            .get_mut(&item_id)
            .unwrap()
            .pending_buy_quantity += item_count;
    }
    if source == Source::Vendor {
        let (items_purchased, purchase_cost, rest) = vendor_purchase.unwrap();
        context.purchases.push((item_id, items_purchased, source));
        if items_purchased > item_count {
            // Should never still have leftovers if we're buying more
            debug_assert!(!context.items.leftovers.contains_key(&item_id));
            context.items.leftovers.insert(
                item_id,
                (items_purchased - item_count, rest, Source::Vendor),
            );
            context
                .lots
                .insert(item_id, (purchase_cost, items_purchased));
        }
    }

    Some(PreciseCraftingCost {
        cost: cost + cost_of_leftovers_used,
        source,
        crafting: crafting_cost,
    })
}

/// Swap in `state`, whose purchases match up to index `start`, keeping the items being crafted;
/// returns the state replaced
fn restore(
    context: &mut PreciseCraftingCostContext,
    tp_listings_map: &mut BTreeMap<u32, trading_post::ItemListings>,
    start: usize,
    mut state: PreciseCraftingCostContext,
) -> PreciseCraftingCostContext {
    let mut pending = |purchases: &[(u32, u32, Source)], add: bool| {
        for &(id, count, source) in purchases {
            if source == Source::TradingPost {
                let pending = &mut tp_listings_map.get_mut(&id).unwrap().pending_buy_quantity;
                *pending = if add {
                    *pending + count
                } else {
                    *pending - count
                };
            }
        }
    };
    pending(&context.purchases[start..], false);
    pending(&state.purchases[start..], true);
    state.being_crafted = std::mem::take(&mut context.being_crafted);
    std::mem::replace(context, state)
}

#[derive(Debug, Eq, PartialEq)]
pub struct PurchasedIngredient {
    pub count: u32,
    pub max_price: Money,
    pub min_price: Money,
    pub total_cost: Money,
}

trait OptionInnerMin<T> {
    fn inner_min(self, other: Option<T>) -> Option<T>;
}

impl<T> OptionInnerMin<T> for Option<T>
where
    T: Ord + Copy,
{
    fn inner_min(self, other: Option<T>) -> Option<T> {
        match (self, other) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (None, _) => other,
            (_, None) => self,
        }
    }
}
