mod common;

use common::{tp_listings_map, MockItem, MockRecipe};

use gw2_arbitrage::{
    api::RecipeIngredient,
    crafting,
    item::Item,
    profit::calculate_crafting_profit,
    recipe::{CurrencyCost, Recipe},
};

use std::collections::HashMap;

#[test]
fn vendor_preferred_over_merchant_recipe_at_equal_cost() {
    let item_id = 1000;
    let jug_of_water_item_id = 12156;

    let mut items_map = HashMap::new();
    items_map.insert(item_id, Item::mock(item_id, "Main item", 0));
    items_map.insert(
        jug_of_water_item_id,
        Item::mock(jug_of_water_item_id, "Jug of Water", 1),
    );

    let mut recipes_map = HashMap::new();
    recipes_map.insert(
        item_id,
        Recipe::mock(
            300,
            item_id,
            1,
            [],
            &[RecipeIngredient {
                item_id: jug_of_water_item_id,
                count: 1,
            }],
            true,
        ),
    );
    let mut jug_recipe = Recipe::mock(301, jug_of_water_item_id, 10, [], &[], true);
    jug_recipe.currencies = vec![CurrencyCost {
        currency_id: 1,
        count: 80,
    }];
    recipes_map.insert(jug_of_water_item_id, jug_recipe);

    let tp_listings_map = tp_listings_map(vec![(item_id, vec![(100, 1)], vec![])]);

    let mut purchased_ingredients = HashMap::new();
    let profitable_item = calculate_crafting_profit(
        item_id,
        &recipes_map,
        &items_map,
        &tp_listings_map,
        Some(&mut purchased_ingredients),
        &Default::default(),
    )
    .unwrap();

    assert!(purchased_ingredients.contains_key(&(jug_of_water_item_id, crafting::Source::Vendor)));
    assert!(!profitable_item
        .crafted_items
        .crafted
        .contains_key(&jug_of_water_item_id));
    assert_eq!(profitable_item.crafting_steps, 1);
}
