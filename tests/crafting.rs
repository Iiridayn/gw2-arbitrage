mod common;

use common::{one_ingredient_game_data, tp_listings_map, MockRecipe};

use gw2_arbitrage::{
    api::RecipeIngredient,
    crafting::{self, CraftedItems},
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

/// Each step's group and each group's label, when crafting one of each recipe
fn groups(item_id: u32, recipes: Vec<Recipe>) -> (Vec<Option<usize>>, Vec<String>) {
    let crafted = CraftedItems {
        crafted: recipes.iter().map(|r| (r.output_item_id, 1)).collect(),
        leftovers: HashMap::new(),
    };
    let recipes_map = recipes.into_iter().map(|r| (r.output_item_id, r)).collect();
    let mut steps = crafted.sorted(item_id, &recipes_map);
    let step_groups = crafting::discipline_groups(&mut steps);
    let labels = crafting::disciplines_labels(&steps, &step_groups);
    (step_groups, labels)
}

#[test]
fn parts_in_a_loop_keep_their_order() {
    // 1 is made from 2, whose recipe would use 1; the 1 for it is bought
    let (step_groups, labels) = groups(
        1,
        vec![
            recipe(1, [Armorsmith], 400, &[2]),
            recipe(2, [Tailor], 300, &[1]),
        ],
    );
    assert_eq!(step_groups, [Some(0), Some(1)]);
    assert_eq!(labels, ["T 300", "Am 400"]);
}

#[test]
fn sub_part_narrows_disciplines() {
    // Superior Rune of the Dragonhunter from Superior Rune of the Guardian
    let (step_groups, labels) = groups(
        1,
        vec![
            recipe(1, [Armorsmith, Leatherworker, Tailor], 400, &[2]),
            recipe(2, [Armorsmith], 300, &[]),
        ],
    );
    assert_eq!(step_groups, [Some(0), Some(0)]);
    assert_eq!(labels, ["Am 400"]);
}

#[test]
fn disjoint_disciplines_need_two_groups() {
    let (step_groups, labels) = groups(
        1,
        vec![
            recipe(1, [Scribe], 400, &[2]),
            recipe(2, [Jeweler], 350, &[]),
        ],
    );
    assert_eq!(step_groups, [Some(0), Some(1)]);
    assert_eq!(labels, ["J 350", "Sr 400"]);
}

#[test]
fn forge_and_salvage_need_no_group() {
    let (step_groups, labels) = groups(
        1,
        vec![
            recipe(1, [MysticForge], 0, &[2, 3]),
            recipe(2, [Armorsmith, Weaponsmith], 400, &[]),
            recipe(3, [Salvage], 0, &[]),
        ],
    );
    assert_eq!(step_groups, [None, Some(0), None]);
    assert_eq!(labels, ["Am/W 400"]);
}

#[test]
fn vendor_preferred_over_merchant_recipe_at_equal_cost() {
    // Jug of Water: 8c from the vendor, or 80c for 10 from a Merchant recipe
    let mut game_data = one_ingredient_game_data(1000, 12156, 1);
    let mut jug_recipe = Recipe::mock(2, 12156, 10, [], &[], true);
    jug_recipe.currencies = vec![CurrencyCost {
        currency_id: 1,
        count: 80,
    }];
    game_data.recipes.insert(12156, jug_recipe);

    let mut purchased_ingredients = HashMap::new();
    let profitable_item = calculate_crafting_profit(
        1000,
        &game_data,
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
    let (step_groups, _) = groups(
        1,
        vec![
            recipe(1, [Artificer], 400, &[2, 3]),
            recipe(2, [Scribe], 400, &[]),
            recipe(3, [Artificer], 400, &[]),
        ],
    );
    assert_eq!(step_groups, [Some(1), Some(0), Some(0)]);
}
