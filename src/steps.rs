use crate::crafting::CraftedItems;
use crate::recipe::{Discipline, Recipe};

use std::collections::HashMap;

impl CraftedItems {
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

    /// The steps under crafting `count` of an item by one of its recipes, crafting the item that
    /// teaches it included, then that step; also returns the items they consume
    fn sort_steps<'a>(
        &self,
        (item_id, index): (u32, usize),
        count: u32,
        recipes_map: &'a HashMap<u32, Vec<Recipe>>,
        being_crafted: &mut Vec<u32>,
    ) -> (u32, Vec<(u32, u32, &'a Recipe)>) {
        let recipe = &recipes_map[&item_id][index];
        being_crafted.push(item_id);
        let unlock_id = self.unlock(recipe).map(|(unlock_id, _)| unlock_id);
        let ingredient_ids = recipe
            .ingredients
            .iter()
            .map(|i| i.item_id)
            .chain(unlock_id);
        let (sum, mut sorted) = self.sort_ingredients(ingredient_ids, recipes_map, being_crafted);
        being_crafted.pop();

        let consumed: u32 = recipe.ingredients.iter().map(|i| i.count).sum();
        sorted.push((item_id, count, recipe));
        (sum + consumed * count / recipe.output_item_count, sorted)
    }

    // TODO: use an iterator? - complains about lifetimes though, despite copy data output w/o references
    pub fn sorted<'a>(
        &self,
        item_ids: &[u32],
        recipes_map: &'a HashMap<u32, Vec<Recipe>>,
    ) -> Vec<(u32, u32, &'a Recipe)> {
        self.sort_ingredients(item_ids.iter().copied(), recipes_map, &mut vec![])
            .1
    }
}

/// Group crafting station steps, starting from the most restrictive step, and
/// reorder the steps to avoid switching back to a group. Returns each step's
/// group index; None for steps not at a station (salvage, double click, …).
pub fn discipline_groups(
    steps: &mut Vec<(u32, u32, &Recipe)>,
    items: &CraftedItems,
) -> Vec<Option<usize>> {
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
    // longer needed, as an ingredient or to learn a recipe, preferring the current group
    let mut remaining: Vec<usize> = (0..steps.len()).collect();
    let mut order = vec![];
    let mut current = None;
    while !remaining.is_empty() {
        let used = |i: usize| {
            let output = steps[i].0;
            let learns = |recipe: &Recipe| {
                let unlock = items.unlock(recipe);
                unlock.is_some_and(|(unlock_id, _)| unlock_id == output)
            };
            let uses = |&j: &usize| {
                let recipe = steps[j].2;
                let ingredient = recipe.ingredients.iter().any(|g| g.item_id == output);
                j != i && (ingredient || learns(recipe))
            };
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
