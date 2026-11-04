mod common;

use common::{one_ingredient_maps, tp_listings_map, MockRecipe};

use gw2_arbitrage::{
    api::RecipeIngredient,
    crafting::{self, CraftedItems, DisciplineGroup},
    profit::calculate_crafting_profit,
    recipe::{
        CurrencyCost,
        Discipline::{self, *},
        Recipe,
    },
};

use std::collections::HashMap;

/// Recipe making one of `output` from one of each ingredient
fn recipe<const A: usize>(
    output: u32,
    disciplines: [Discipline; A],
    min_rating: u16,
    ingredients: &[u32],
) -> Recipe {
    let ingredients: Vec<_> = ingredients
        .iter()
        .map(|&item_id| RecipeIngredient { item_id, count: 1 })
        .collect();
    let mut recipe = Recipe::mock(output, output, 1, disciplines, &ingredients, true);
    recipe.min_rating = Some(min_rating);
    recipe
}

/// Discipline groups and each step's group, when crafting one of each recipe
fn groups(item_id: u32, recipes: Vec<Recipe>) -> (Vec<DisciplineGroup>, Vec<Option<usize>>) {
    let crafted = CraftedItems {
        crafted: recipes.iter().map(|r| (r.output_item_id, 1)).collect(),
        leftovers: HashMap::new(),
    };
    let recipes_map = recipes.into_iter().map(|r| (r.output_item_id, r)).collect();
    crafting::discipline_groups(&mut crafted.sorted(item_id, &recipes_map))
}

#[test]
fn sub_part_narrows_disciplines() {
    // Superior Rune of the Dragonhunter from Superior Rune of the Guardian
    let (groups, step_groups) = groups(
        1,
        vec![
            recipe(1, [Armorsmith, Leatherworker, Tailor], 400, &[2]),
            recipe(2, [Armorsmith], 300, &[]),
        ],
    );
    assert_eq!(groups, [(vec![Armorsmith], Some(400))]);
    assert_eq!(step_groups, [Some(0), Some(0)]);
}

#[test]
fn disjoint_disciplines_need_two_groups() {
    let (groups, step_groups) = groups(
        1,
        vec![
            recipe(1, [Scribe], 400, &[2]),
            recipe(2, [Jeweler], 350, &[]),
        ],
    );
    assert_eq!(
        groups,
        [(vec![Jeweler], Some(350)), (vec![Scribe], Some(400))]
    );
    assert_eq!(step_groups, [Some(0), Some(1)]);
}

#[test]
fn forge_and_salvage_need_no_group() {
    let (groups, step_groups) = groups(
        1,
        vec![
            recipe(1, [MysticForge], 0, &[2, 3]),
            recipe(2, [Armorsmith, Weaponsmith], 400, &[]),
            recipe(3, [Salvage], 0, &[]),
        ],
    );
    assert_eq!(groups, [(vec![Armorsmith, Weaponsmith], Some(400))]);
    assert_eq!(step_groups, [None, Some(0), None]);
}

#[test]
fn vendor_preferred_over_merchant_recipe_at_equal_cost() {
    // Jug of Water: 8c from the vendor, or 80c for 10 from a Merchant recipe
    let (items_map, mut recipes_map) = one_ingredient_maps(1000, 12156, 1);
    let mut jug_recipe = Recipe::mock(2, 12156, 10, [], &[], true);
    jug_recipe.currencies = vec![CurrencyCost {
        currency_id: 1,
        count: 80,
    }];
    recipes_map.insert(12156, jug_recipe);

    let mut purchased_ingredients = HashMap::new();
    let profitable_item = calculate_crafting_profit(
        1000,
        &recipes_map,
        &items_map,
        &tp_listings_map(vec![(1000, vec![(100, 1)], vec![])]),
        Some(&mut purchased_ingredients),
        &Default::default(),
    )
    .unwrap();

    assert!(purchased_ingredients.contains_key(&(12156, crafting::Source::Vendor)));
    assert_eq!(profitable_item.crafting_steps, 1);
}

#[test]
fn hand_off_part_is_made_first() {
    // The sort puts the Artificer part (3) before the Scribe part (2)
    let (_, step_groups) = groups(
        1,
        vec![
            recipe(1, [Artificer], 400, &[2, 3]),
            recipe(2, [Scribe], 400, &[]),
            recipe(3, [Artificer], 400, &[]),
        ],
    );
    assert_eq!(step_groups, [Some(1), Some(0), Some(0)]);
}
