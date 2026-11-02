mod common;

use common::{one_ingredient_maps, tp_listings_map, MockRecipe};

use gw2_arbitrage::{
    crafting,
    profit::calculate_crafting_profit,
    recipe::{CurrencyCost, Recipe},
};

use std::collections::HashMap;

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
