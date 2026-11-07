use crate::api;
use crate::config;
use crate::item::Item;
use crate::money::Money;
use crate::recipe::{Discipline, Recipe};
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
    pub recipes: HashMap<u32, Recipe>, // output item id, recipe
    pub items: HashMap<u32, Item>,
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
        tp_prices_map: &HashMap<u32, api::Price>,
        opt: &config::CraftingOptions,
    ) -> RequiredUnlocks {
        crafted_items
            .unknown_recipes(&game_data.recipes, self.known_recipes)
            .into_iter()
            .map(|recipe_id| {
                let unlock = self.cheapest_unlock(recipe_id, game_data, tp_prices_map, opt);
                (recipe_id, unlock)
            })
            .collect()
    }

    /// Cost of a recipe's cheapest unlock item; zero if the recipe is known
    fn cost(
        &self,
        recipe: &Recipe,
        game_data: &GameData,
        tp_prices_map: &HashMap<u32, api::Price>,
        opt: &config::CraftingOptions,
    ) -> Option<Money> {
        match recipe.id {
            Some(id) if !recipe.is_automatic() && !self.known_recipes.contains(&id) => self
                .cheapest_unlock(id, game_data, tp_prices_map, opt)
                .map(|(_, cost)| cost),
            _ => Some(Money::zero()),
        }
    }

    fn cheapest_unlock(
        &self,
        recipe_id: u32,
        game_data: &GameData,
        tp_prices_map: &HashMap<u32, api::Price>,
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
                    tp_prices_map,
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
    tp_prices_map: &HashMap<u32, api::Price>,
    unlocks: Option<&RecipeUnlocks>,
    being_crafted: &mut Vec<u32>,
    opt: &config::CraftingOptions,
) -> Option<EstimatedCraftingCost> {
    let item = game_data.items.get(&item_id);
    let recipe = game_data
        .recipes
        .get(&item_id)
        .filter(|_| !being_crafted.contains(&item_id));

    being_crafted.push(item_id);
    let crafting_cost = recipe.and_then(|recipe| {
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
                    tp_prices_map,
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
                Some(unlocks) => unlocks.cost(recipe, game_data, tp_prices_map, opt)?,
                None => Money::zero(),
            };
            let (share, _leftovers) = cost.split(count, batches * recipe.output_item_count);
            Some(share + unlock_cost)
        }
    });
    being_crafted.pop();

    let tp_cost = tp_prices_map
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

pub struct PreciseCraftingCostContext {
    pub purchases: Vec<(u32, u32, Source)>, // id, count, Source
    pub items: CraftedItems,
    pub being_crafted: Vec<u32>,          // outermost first
    pub lots: HashMap<u32, (Money, u32)>, // id, (cost, count) of its leftovers' batches or lot
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct CraftedItems {
    pub crafted: HashMap<u32, u32>,                    // id, count
    pub leftovers: HashMap<u32, (u32, Money, Source)>, // id, (count, total cost, source)
}

impl CraftedItems {
    pub fn crafting_steps(&self, recipes_map: &HashMap<u32, Recipe>) -> Ratio<u32> {
        let total_crafting_steps = self
            .crafted
            .iter()
            .map(|(item_id, &count)| {
                let recipe = recipes_map.get(item_id);
                let output_item_count = recipe.map(|recipe| recipe.output_item_count).unwrap_or(1);
                Ratio::new(count, output_item_count)
            })
            .reduce(|total, count| total + count)
            .unwrap();
        debug_assert!(total_crafting_steps.is_integer());
        total_crafting_steps
    }

    /// Estimated seconds to craft everything, with one batch per crafted item
    pub fn crafting_seconds(&self, item_id: u32, recipes_map: &HashMap<u32, Recipe>) -> f64 {
        let crafting: f64 = self
            .crafted
            .iter()
            .filter_map(|(item_id, &count)| {
                let recipe = recipes_map.get(item_id)?;
                Some(recipe.crafting_seconds(count / recipe.output_item_count))
            })
            .sum();
        let step_groups = discipline_groups(&mut self.sorted(item_id, recipes_map));
        let groups: Vec<usize> = step_groups.into_iter().flatten().collect();
        let switches = groups.windows(2).filter(|w| w[0] != w[1]).count();
        crafting + switches as f64 * CHARACTER_SWITCH_SECONDS
    }

    pub fn unknown_recipes(
        &self,
        recipes_map: &HashMap<u32, Recipe>,
        known_recipes: &HashSet<u32>,
    ) -> HashSet<u32> {
        let mut unknown_recipes = HashSet::new();
        for item_id in self.crafted.keys() {
            let recipe = if let Some(recipe) = recipes_map.get(item_id) {
                recipe
            } else {
                continue;
            };
            if let Some(id) = recipe.id {
                if !recipe.is_automatic() && !known_recipes.contains(&id) {
                    unknown_recipes.insert(id);
                }
            }
        }
        unknown_recipes
    }

    /// Sort in the order which will remove ingredients from the inventory fastest; also returns
    /// the items consumed by this step and the steps under it
    fn sort_ingredients<'a>(
        &self,
        item_id: u32,
        count: u32,
        recipes_map: &'a HashMap<u32, Recipe>,
        being_crafted: &mut Vec<u32>,
    ) -> (u32, Vec<(u32, u32, &'a Recipe)>) {
        let mut ingredients = Vec::new();
        let recipe = recipes_map.get(&item_id).unwrap();
        being_crafted.push(item_id);
        for ingredient in &recipe.ingredients {
            let crafted = self.crafted.get(&ingredient.item_id).unwrap_or(&0);
            if *crafted == 0 || being_crafted.contains(&ingredient.item_id) {
                continue;
            }
            // Let each recipe which uses something get full credit for it; then
            // only add it once to the crafting list once weights are determined
            ingredients.push((
                ingredient.item_id,
                self.sort_ingredients(ingredient.item_id, *crafted, recipes_map, being_crafted),
            ));
        }
        being_crafted.pop();

        // Put first the ingredient whose steps consume the most items, to empty the inventory
        // fastest; ties go to the higher item id
        ingredients.sort_by(|(a_id, (a_sum, _)), (b_id, (b_sum, _))| {
            b_sum.cmp(a_sum).then(b_id.cmp(a_id))
        });

        let mut sorted = Vec::new();
        let consumed: u32 = recipe.ingredients.iter().map(|i| i.count).sum();
        let mut sum = consumed * count / recipe.output_item_count;
        let mut used = HashSet::new();
        for ingredient in ingredients.iter() {
            sum += ingredient.1 .0;
            for &subingredient in ingredient.1 .1.iter() {
                if used.contains(&subingredient.0) {
                    continue;
                }
                used.insert(subingredient.0);
                sorted.push(subingredient);
            }
        }
        sorted.push((item_id, count, recipe));
        (sum, sorted)
    }

    // TODO: use an iterator? - complains about lifetimes though, despite copy data output w/o references
    // TODO: unwrap??
    pub fn sorted<'a>(
        &self,
        item_id: u32,
        recipes_map: &'a HashMap<u32, Recipe>,
    ) -> Vec<(u32, u32, &'a Recipe)> {
        let count = *self.crafted.get(&item_id).unwrap();
        self.sort_ingredients(item_id, count, recipes_map, &mut vec![])
            .1
    }
}

/// Group crafting station steps, starting from the most restrictive step, and
/// reorder the steps to avoid switching back to a group. Returns each step's
/// group index; None for steps not at a crafting station (forge, salvage, …).
pub fn discipline_groups(steps: &mut Vec<(u32, u32, &Recipe)>) -> Vec<Option<usize>> {
    let disciplines = |recipe: &Recipe| -> Vec<Discipline> {
        let crafting = recipe.disciplines.iter().copied();
        crafting.filter(Discipline::is_crafting).collect()
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
            shared.retain(|d| d.is_crafting() && recipes.iter().all(|r| r.disciplines.contains(d)));
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
        let chunk =
            price_from_one_source(item_id, count, game_data, tp_listings_map, context, opt)?;
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
    context: &mut PreciseCraftingCostContext,
    opt: &config::CraftingOptions,
) -> Option<PreciseCraftingCost> {
    let item = game_data.items.get(&item_id);
    let recipe = game_data
        .recipes
        .get(&item_id)
        .filter(|_| !context.being_crafted.contains(&item_id));
    let output_item_count = recipe.map(|recipe| recipe.output_item_count).unwrap_or(1);

    let purchases_ptr = context.purchases.len();
    let crafted_backup = context.items.crafted.clone();

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

    // Craft x, but stash the rest; price is the fraction though
    let crafting_count = Ratio::new(item_count, output_item_count)
        .ceil()
        .to_integer();
    let output_count = crafting_count * output_item_count;

    let leftovers_backup = context.items.leftovers.clone();
    let lots_backup = context.lots.clone();
    context.being_crafted.push(item_id);
    let batches_cost = recipe.and_then(|recipe| {
        if !opt.include_timegated && recipe.is_timegated() {
            return None;
        }

        let mut cost = recipe.currency_cost()? * crafting_count;
        for ingredient in &recipe.ingredients {
            // adjust ingredient count based on fraction of parent recipe that was requested
            let ingredient_count = ingredient.count * crafting_count;
            let ingredient_cost = calculate_precise_min_crafting_cost(
                ingredient.item_id,
                ingredient_count,
                game_data,
                tp_listings_map,
                context,
                opt,
            );
            if let Some(PreciseCraftingCost {
                cost: ingredient_cost,
                ..
            }) = ingredient_cost
            {
                cost += ingredient_cost;
            } else {
                return None;
            }
        }

        Some(cost)
    });
    context.being_crafted.pop();
    let (crafting_cost, crafting_leftovers_cost) = batches_cost
        .map(|cost| cost.split(item_count, output_count))
        .unzip();

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
        *context.items.crafted.entry(item_id).or_insert(0) += output_count;
        if output_count > item_count {
            // Should never have leftovers if we're crafting more
            debug_assert!(!context.items.leftovers.contains_key(&item_id));
            context.items.leftovers.insert(
                item_id,
                (
                    output_count - item_count,
                    crafting_leftovers_cost.unwrap(),
                    Source::Crafting,
                ),
            );
            context
                .lots
                .insert(item_id, (batches_cost.unwrap(), output_count));
        }
    } else {
        // Un-mark ingredients for purchase
        for (purchase_id, purchase_quantity, purchase_source) in
            context.purchases.drain(purchases_ptr..)
        {
            if purchase_source == Source::TradingPost {
                tp_listings_map
                    .get_mut(&purchase_id)
                    .unwrap()
                    .pending_buy_quantity -= purchase_quantity;
            }
        }
        context.items.crafted = crafted_backup;
        context.items.leftovers = leftovers_backup;
        context.lots = lots_backup;
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
