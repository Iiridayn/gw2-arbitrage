mod common;

use common::{one_ingredient_game_data, tp_listings_map, MockRecipe};

use gw2_arbitrage::{
    api::{Price, PriceInfo, RecipeIngredient},
    crafting::{self, CraftedItems, GameData, SummaryPrices},
    money::Money,
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
        crafted: recipes.iter().map(|r| ((r.output_item_id, 0), 1)).collect(),
        leftovers: HashMap::new(),
    };
    let recipes_map = GameData::new(recipes, vec![]).recipes;
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
fn mystic_forge_is_a_group_and_salvage_needs_none() {
    let mut forge = recipe(1, [MysticForge], 0, &[2, 3]);
    forge.min_rating = None;
    let (step_groups, labels) = groups(
        1,
        vec![
            forge,
            recipe(2, [Armorsmith, Weaponsmith], 400, &[]),
            recipe(3, [Salvage], 0, &[]),
        ],
    );
    assert_eq!(step_groups, [Some(1), None, Some(0)]);
    assert_eq!(labels, ["MF", "Am/W 400"]);
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
    game_data.recipes.insert(12156, vec![jug_recipe]);

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
fn mystic_forge_steps_are_done_in_one_visit() {
    // 1 is made from 2 and 3, each made from a Mystic Forge part
    let recipes = [
        recipe(1, [Artificer], 400, &[2, 3]),
        recipe(2, [Artificer], 400, &[4]),
        recipe(3, [Artificer], 400, &[5]),
        recipe(4, [MysticForge], 0, &[]),
        recipe(5, [MysticForge], 0, &[]),
    ];
    let mut steps: Vec<_> = [4, 2, 5, 3, 1]
        .iter()
        .map(|&id| (id, 1, &recipes[id as usize - 1]))
        .collect();
    crafting::discipline_groups(&mut steps);
    let ids: Vec<_> = steps.iter().map(|step| step.0).collect();
    assert_eq!(ids, [4, 5, 2, 3, 1]);
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

#[test]
fn lower_bound_settles_under_a_promotion_that_uses_its_own_output() {
    // 250 Copper Ore, 1 Iron Ore and 5 dust make 88 Iron Ore
    let ingredients =
        [(2, 250), (1, 1), (3, 5)].map(|(item_id, count)| RecipeIngredient { item_id, count });
    let game_data = GameData::new([Recipe::mock(1, 1, 88, [], &ingredients, true)], vec![]);
    let prices = HashMap::from([listed(1, 100), listed(2, 1), listed(3, 1)]);
    let prices = SummaryPrices::new(prices, &game_data);
    // ⌊(250 + 2 + 5) / 88⌋, under Iron Ore's 100 listing and its crafted 355 / 88
    assert_eq!(prices.lower_bounds[&1], Money::from_copper(2));
}

/// Summary price of an item listed at `unit_price`, with no buy orders
fn listed(id: u32, unit_price: u32) -> (u32, Price) {
    let sells = PriceInfo {
        unit_price,
        quantity: 1000,
    };
    let buys = PriceInfo {
        unit_price: 0,
        quantity: 0,
    };
    (id, Price { id, buys, sells })
}

#[test]
fn game_data_keeps_the_first_of_recipes_that_make_the_same() {
    let ingredients =
        |pairs: [(u32, u32); 2]| pairs.map(|(item_id, count)| RecipeIngredient { item_id, count });
    let recipes = [
        Recipe::mock(1, 10, 1, [Armorsmith], &ingredients([(2, 1), (3, 5)]), true),
        Recipe::mock(2, 10, 1, [Armorsmith], &ingredients([(3, 5), (2, 1)]), true),
        Recipe::mock(3, 10, 1, [Tailor], &ingredients([(2, 1), (3, 5)]), true),
        Recipe::mock(4, 10, 2, [Armorsmith], &ingredients([(2, 1), (3, 5)]), true),
    ];
    let game_data = GameData::new(recipes, vec![]);
    let ids: Vec<_> = game_data.recipes[&10].iter().map(|r| r.id).collect();
    assert_eq!(ids, [Some(1), Some(3), Some(4)]);
}

#[test]
fn estimate_takes_the_cheaper_recipe() {
    let one = |item_id| [RecipeIngredient { item_id, count: 1 }];
    let recipes = [
        Recipe::mock(1, 1, 1, [], &one(2), true),
        Recipe::mock(2, 1, 1, [], &one(3), true),
    ];
    let game_data = GameData::new(recipes, vec![]);
    let prices = SummaryPrices::new(HashMap::from([listed(2, 100), listed(3, 50)]), &game_data);
    let estimate = crafting::calculate_estimated_min_crafting_cost(
        1,
        1,
        &game_data,
        &prices,
        None,
        &mut vec![],
        &Default::default(),
    );
    assert_eq!(estimate.unwrap().cost, Money::from_copper(50));
}

#[test]
fn steps_list_each_recipe_of_an_item() {
    let crafted = CraftedItems {
        crafted: HashMap::from([((1, 0), 2), ((2, 0), 1), ((2, 1), 1)]),
        leftovers: HashMap::new(),
    };
    let recipes = vec![
        recipe(1, [Artificer], 400, &[2]),
        recipe(2, [Armorsmith], 400, &[]),
        recipe(2, [Tailor], 400, &[]),
    ];
    let recipes_map = GameData::new(recipes, vec![]).recipes;
    let steps = crafted.sorted(1, &recipes_map);
    let steps: Vec<_> = steps
        .iter()
        .map(|&(id, n, r)| (id, n, r.disciplines.clone()))
        .collect();
    assert_eq!(
        steps,
        [
            (2, 1, vec![Tailor]),
            (2, 1, vec![Armorsmith]),
            (1, 2, vec![Artificer])
        ]
    );
}
