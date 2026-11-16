use crate::api;
use crate::config;
use crate::item::Item;
use crate::money::Money;
use crate::recipe::{self, Recipe};
use crate::steps;
use crate::trading_post;

use num_rational::Ratio;
use num_traits::Zero;

use std::collections::{BTreeMap, HashMap, HashSet};

/// Logging out and in to use another character's crafting disciplines
const CHARACTER_SWITCH_MS: u32 = 30_000;

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
    pub unlocked_by: HashMap<u32, Vec<u32>>, // recipe id, the items that unlock it
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
        let mut unlocked_by: HashMap<u32, Vec<u32>> = HashMap::new();
        for item in &items {
            for recipe_id in item.recipe_unlocks().into_iter().flatten() {
                unlocked_by.entry(recipe_id).or_default().push(item.id);
            }
        }
        // so ties between unlock items always go the same way
        unlocked_by.values_mut().for_each(|ids| ids.sort_unstable());
        GameData {
            recipes: recipes_map,
            items: items.into_iter().map(|item| (item.id, item)).collect(),
            unlocked_by,
        }
    }

    /// Every item crafting `item_ids` can touch, by any of each item's recipes, each once and in
    /// order. Given `known_recipes`, also the unlock items of the recipes not in it, and their
    /// trees, as the plan may buy or craft them. Both the listings to fetch and the lower bounds
    /// need all of these.
    pub fn collect_tree_ids(
        &self,
        item_ids: &[u32],
        known_recipes: Option<&HashSet<u32>>,
    ) -> Vec<u32> {
        let (mut ids, mut pending) = (HashSet::new(), item_ids.to_vec());
        while let Some(id) = pending.pop() {
            if !ids.insert(id) {
                continue;
            }
            for recipe in self.recipes.get(&id).into_iter().flatten() {
                pending.extend(recipe.ingredients.iter().map(|i| i.item_id));
                if let Some(id) = known_recipes.and_then(|known| recipe.filter_unknown_id(known)) {
                    pending.extend(self.unlocked_by.get(&id).into_iter().flatten());
                }
            }
        }
        let mut ids: Vec<u32> = ids.into_iter().collect();
        ids.sort_unstable();
        ids
    }

    pub fn item_name(&self, id: u32) -> String {
        let item = self.items.get(&id);
        item.map_or_else(|| "???".to_string(), |item| item.to_string())
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
    let per_item = |cost: Money, n: u32| {
        let (_, smallest) = cost.split(n - 1, n);
        smallest
    };
    for item in ids.clone().filter_map(|id| game_data.items.get(&id)) {
        if let Some((lot_cost, lot_size)) = item.vendor_lot() {
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

/// Recipe id to the unlock item bought or crafted for it and its cost, or None if none could be
/// priced
pub type RecipeUnlocks = BTreeMap<u32, Option<(u32, Money)>>;

/// Calculate the lowest cost method to obtain count of the given item, using only the current
/// high/low tp prices. This may involve a combination of crafting, trading and buying from vendors.
pub fn calculate_estimated_min_crafting_cost(
    item_id: u32,
    count: u32,
    game_data: &GameData,
    prices: &SummaryPrices,
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

    let vendor_cost = item.and_then(Item::vendor_lot).map(|(lot_cost, lot_size)| {
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
        let batches = count.div_ceil(recipe.output_item_count);
        let cost = price_batches(recipe, batches, opt, |id, count| {
            let estimate = calculate_estimated_min_crafting_cost(
                id,
                count,
                game_data,
                prices,
                being_crafted,
                opt,
            );
            Some(estimate?.cost)
        })?;
        let (share, _leftovers) = cost.split(count, batches * recipe.output_item_count);
        Some(share)
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

    let (cost, source) = cheapest_source(tp_cost, vendor_cost, crafting_cost)?;
    Some(EstimatedCraftingCost { cost, source })
}

/// What `batches` of `recipe` cost: its currencies, and each ingredient as `price` prices it; None
/// if one can't be priced, or the recipe is timegated and those aren't included
fn price_batches(
    recipe: &Recipe,
    batches: u32,
    opt: &config::CraftingOptions,
    mut price: impl FnMut(u32, u32) -> Option<Money>,
) -> Option<Money> {
    let cost = recipe.currency_cost();
    let cost = cost.filter(|_| opt.include_timegated || !recipe.is_timegated())?;
    let mut ingredients = recipe.ingredients.iter();
    ingredients.try_fold(cost * batches, |cost, ingredient| {
        Some(cost + price(ingredient.item_id, ingredient.count * batches)?)
    })
}

/// The cheapest source and its cost; equal costs go to the TP, then the vendor, then crafting
fn cheapest_source(
    tp: Option<Money>,
    vendor: Option<Money>,
    crafting: Option<Money>,
) -> Option<(Money, Source)> {
    let cost = tp.inner_min(vendor).inner_min(crafting)?;
    let source = if tp.is_some_and(|tp| tp.cmp(&cost).is_eq()) {
        Source::TradingPost
    } else if vendor.is_some_and(|vendor| vendor.cmp(&cost).is_eq()) {
        Source::Vendor
    } else {
        Source::Crafting
    };
    Some((cost, source))
}

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
    pub being_crafted: Vec<u32>, // outermost first
    pub crafting_ms: u32,        // for ties between recipes
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct CraftedItems {
    pub crafted: HashMap<(u32, usize), u32>, // (id, recipe index), count
    pub leftovers: HashMap<u32, (u32, Money, Source)>, // id, (count, total cost, source)
    pub lots: HashMap<u32, (Money, u32)>,    // id, (cost, count) of its leftovers' batches or lot
    pub unlocks: RecipeUnlocks,
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
    pub fn crafting_seconds(
        &self,
        item_ids: &[u32],
        recipes_map: &HashMap<u32, Vec<Recipe>>,
    ) -> f64 {
        let crafting: u32 = self
            .crafted
            .iter()
            .map(|(&(item_id, index), &count)| {
                let recipe = &recipes_map[&item_id][index];
                recipe.crafting_ms(count / recipe.output_item_count)
            })
            .sum();
        let step_groups = steps::discipline_groups(&mut self.sorted(item_ids, recipes_map), self);
        let groups: Vec<usize> = step_groups.into_iter().flatten().collect();
        let switches = groups.windows(2).filter(|w| w[0] != w[1]).count() as u32;
        (crafting + switches * CHARACTER_SWITCH_MS) as f64 / 1000.0
    }

    pub fn unknown_recipes(
        &self,
        recipes_map: &HashMap<u32, Vec<Recipe>>,
        known_recipes: &HashSet<u32>,
    ) -> HashSet<u32> {
        let mut unknown_recipes = HashSet::new();
        for &(item_id, index) in self.crafted.keys() {
            let recipe = &recipes_map[&item_id][index];
            unknown_recipes.extend(recipe.filter_unknown_id(known_recipes));
        }
        unknown_recipes
    }

    /// The unlock item obtained to learn `recipe`, and its cost
    pub fn unlock(&self, recipe: &Recipe) -> Option<(u32, Money)> {
        *self.unlocks.get(&recipe.id?)?
    }

    /// Keep what's left of a lot of `count` costing `cost` once `used` are taken, and the lot, which
    /// later takes split the cost by
    fn keep_leftovers(
        &mut self,
        item_id: u32,
        used: u32,
        (cost, count): (Money, u32),
        source: Source,
    ) {
        if count > used {
            // Should never have leftovers if we're getting more
            debug_assert!(!self.leftovers.contains_key(&item_id));
            let (_, rest) = cost.split(used, count);
            self.leftovers.insert(item_id, (count - used, rest, source));
            self.lots.insert(item_id, (cost, count));
        }
    }

    /// Take up to `count` of `item_id` from the leftovers: how many, their cost and their source
    fn take_leftovers(&mut self, item_id: u32, count: u32) -> Option<(u32, Money, Source)> {
        let (left, cost, source) = self.leftovers.remove(&item_id)?;
        if left <= count {
            self.lots.remove(&item_id);
            return Some((left, cost, source));
        }
        // split the lot by all of it used so far, so the shares don't depend on how many were
        // taken at a time
        let (lot_cost, lot_count) = self.lots[&item_id];
        let (_, rest) = lot_cost.split(lot_count - left + count, lot_count);
        self.leftovers.insert(item_id, (left - count, rest, source));
        Some((count, cost - rest, source))
    }
}

/// The precise cost of one more of `item_id` on top of the plan's `items`, and the state it would
/// leave; the listings it reserves are released, as it's one candidate of several
pub fn price_one_more(
    item_id: u32,
    game_data: &GameData,
    listings: &mut BTreeMap<u32, trading_post::ItemListings>,
    lower_bounds: &HashMap<u32, Money>,
    items: &CraftedItems,
    opt: &config::CraftingOptions,
) -> (Option<PreciseCraftingCost>, PreciseCraftingCostContext) {
    let mut context = PreciseCraftingCostContext {
        purchases: vec![],
        items: items.clone(),
        being_crafted: vec![],
        crafting_ms: 0,
    };
    let priced = calculate_precise_min_crafting_cost(
        item_id,
        1,
        game_data,
        listings,
        lower_bounds,
        &mut context,
        opt,
    );
    adjust_pending(&context.purchases, listings, Pending::Release);
    (priced, context)
}

/// Calculate the lowest cost method to obtain the given item, with simulated purchases from
/// the trading post. Each TP price level is priced separately, so cheap listings can be bought
/// and the rest crafted. The source is Crafting only if every level was crafted.
pub fn calculate_precise_min_crafting_cost(
    item_id: u32,
    item_count: u32,
    game_data: &GameData,
    listings: &mut BTreeMap<u32, trading_post::ItemListings>,
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
        let level = listings
            .get(&item_id)
            .and_then(|item_listings| item_listings.lowest_price_quantity());
        let count = level.map_or(remaining, |quantity| quantity.min(remaining));
        let chunk = price_from_one_source(
            item_id,
            count,
            game_data,
            listings,
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

/// Price all of item_count from the cheapest single source
fn price_from_one_source(
    item_id: u32,
    item_count: u32,
    game_data: &GameData,
    listings: &mut BTreeMap<u32, trading_post::ItemListings>,
    lower_bounds: &HashMap<u32, Money>,
    context: &mut PreciseCraftingCostContext,
    opt: &config::CraftingOptions,
) -> Option<PreciseCraftingCost> {
    let item = game_data.items.get(&item_id);
    let recipes = game_data
        .recipes
        .get(&item_id)
        .filter(|_| !context.being_crafted.contains(&item_id));

    let (item_count, cost_of_leftovers_used) =
        match context.items.take_leftovers(item_id, item_count) {
            Some((taken, cost, source)) if taken == item_count => {
                return Some(PreciseCraftingCost {
                    cost,
                    source,
                    crafting: None,
                })
            }
            Some((taken, cost, _)) => (item_count - taken, cost),
            None => (item_count, Money::zero()),
        };

    // buying is priced first, so a recipe whose lower bound can't beat buying is skipped; never for
    // the item being priced itself, whose crafting cost is shown even when buying is cheaper
    let tp_cost = listings
        .get(&item_id)
        .and_then(|item_listings| item_listings.lowest_sell_offer(item_count))
        .map(|offer| Money::from_copper(offer as i32));

    // this count's share of the whole lots bought, and those lots' cost and count
    let (vendor_cost, vendor_lots) = item
        .and_then(Item::vendor_lot)
        .map(|(lot_cost, lot_size)| {
            let lots = item_count.div_ceil(lot_size);
            let (cost, count) = (lot_cost * lots, lots * lot_size);
            let (share, _) = cost.split(item_count, count);
            (share, (cost, count))
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
    let purchases_start = context.purchases.len();
    let start = context.clone();
    // (share, ms, index), batches' cost, state
    let mut best: Option<((Money, u32, usize), Money, PreciseCraftingCostContext)> = None;
    context.being_crafted.push(item_id);
    for (index, recipe) in tried {
        let batches = item_count.div_ceil(recipe.output_item_count);
        let output_count = batches * recipe.output_item_count;

        // skip what can't win; an equal bound is still tried, so time can break the tie
        let skip = bounds[index].is_some_and(|bound| {
            buying.is_some_and(|buying| bound >= buying)
                || best
                    .as_ref()
                    .is_some_and(|((share, ..), ..)| bound > *share)
        });
        let made_so_far = context.items.crafted.get(&(item_id, index)).copied();
        let would_make = made_so_far.unwrap_or(0) + output_count;
        let too_many = opt
            .caps
            .get(&(item_id, index))
            .is_some_and(|&cap| would_make > cap);
        if skip || too_many {
            continue;
        }

        // whole batches; pricing them buys ingredients and crafts sub-items, changing the state
        let cost = price_batches(recipe, batches, opt, |id, count| {
            let precise = calculate_precise_min_crafting_cost(
                id,
                count,
                game_data,
                listings,
                lower_bounds,
                context,
                opt,
            );
            Some(precise?.cost)
        });

        // its crafting time: only the extra batches if it's already in the plan
        let done = context.items.crafted.get(&(item_id, index));
        let done = done.map(|count| count / recipe.output_item_count);
        let spent = done.map_or(0, |done| recipe.crafting_ms(done));
        context.crafting_ms += recipe.crafting_ms(done.unwrap_or(0) + batches) - spent;
        // and each thing not already on the shopping list must be found and bought
        let (before, bought) = context.purchases.split_at(purchases_start);
        let shopping = bought.iter().enumerate().filter(|&(i, &(id, _, source))| {
            let same = |&(other, _, other_source): &(u32, u32, Source)| {
                (other, other_source) == (id, source)
            };
            !before.iter().any(same) && !bought[..i].iter().any(same)
        });
        let shopping = shopping.count() as u32 * recipe::INGREDIENT_SWITCH_MS;
        let ms = context.crafting_ms - start.crafting_ms + shopping;

        // back to the start, keeping this attempt if it beats the best on (share, ms, index)
        let attempt = restore(context, listings, purchases_start, start.clone());
        if let Some(cost) = cost {
            let (share, _) = cost.split(item_count, output_count);
            let key = (share, ms, index);
            if best.as_ref().is_none_or(|(best_key, ..)| key < *best_key) {
                best = Some((key, cost, attempt));
            }
        }
    }
    context.being_crafted.pop();
    let crafting_cost = best.as_ref().map(|((share, ..), ..)| *share);
    let (cost, source) = cheapest_source(tp_cost, vendor_cost, crafting_cost)?;

    if source == Source::Crafting {
        // put the winner's state back
        let ((_, _, index), batches_cost, attempt) = best.unwrap();
        restore(context, listings, purchases_start, attempt);
        let output_item_count = recipes.unwrap()[index].output_item_count;
        let output_count = item_count.div_ceil(output_item_count) * output_item_count;
        *context.items.crafted.entry((item_id, index)).or_insert(0) += output_count;
        let batches = (batches_cost, output_count);
        context
            .items
            .keep_leftovers(item_id, item_count, batches, Source::Crafting);
    }

    // Mark for purchase
    if source == Source::TradingPost {
        context.purchases.push((item_id, item_count, source));
        listings.get_mut(&item_id).unwrap().pending_buy_quantity += item_count;
    }
    if source == Source::Vendor {
        let lots @ (_, lots_count) = vendor_lots.unwrap();
        context.purchases.push((item_id, lots_count, source));
        context
            .items
            .keep_leftovers(item_id, item_count, lots, Source::Vendor);
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
    listings: &mut BTreeMap<u32, trading_post::ItemListings>,
    start: usize,
    mut state: PreciseCraftingCostContext,
) -> PreciseCraftingCostContext {
    adjust_pending(&context.purchases[start..], listings, Pending::Release);
    adjust_pending(&state.purchases[start..], listings, Pending::Hold);
    state.being_crafted = std::mem::take(&mut context.being_crafted);
    std::mem::replace(context, state)
}

enum Pending {
    Hold,
    Release,
}

fn adjust_pending(
    purchases: &[(u32, u32, Source)],
    listings: &mut BTreeMap<u32, trading_post::ItemListings>,
    action: Pending,
) {
    for &(id, count, source) in purchases {
        if source == Source::TradingPost {
            let listing = listings
                .get_mut(&id)
                .unwrap_or_else(|| panic!("Missing listings for ingredient {}", id));
            let held = listing.pending_buy_quantity;
            listing.pending_buy_quantity = match action {
                Pending::Hold => held + count,
                Pending::Release => held - count,
            };
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct PurchasedIngredient {
    pub count: u32,
    pub max_price: Money,
    pub min_price: Money,
    pub total_cost: Money,
}

/// The shopping list: each ingredient bought, by id and source
pub type Purchases = HashMap<(u32, Source), PurchasedIngredient>;

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
