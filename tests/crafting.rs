mod common;

use common::{api_listings, one_ingredient_game_data, MockRecipe};

use gw2_arbitrage::{
    api::{Price, PriceInfo, RecipeIngredient},
    config::CraftingOptions,
    crafting::{self, GameData, SummaryPrices},
    money::Money,
    profit::calculate_crafting_profit,
    recipe::{CurrencyCost, Discipline::*, Recipe},
};

use std::collections::HashMap;

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

    let profitable_item = calculate_crafting_profit(
        &[1000],
        &game_data,
        &api_listings(vec![(1000, vec![(100, 1)], vec![])]),
        None,
        &Default::default(),
    )
    .unwrap();

    let purchases = &profitable_item.purchases;
    assert!(purchases.contains_key(&(12156, crafting::Source::Vendor)));
    assert_eq!(profitable_item.crafting_steps, 1);
}

#[test]
fn equal_costs_are_bought_on_the_tp_before_a_vendor() {
    // Jug of Water: 8c from the vendor, in 10s, and listed at 8c
    let game_data = one_ingredient_game_data(1000, 12156, 1);
    let api_listings = api_listings(vec![
        (12156, vec![], vec![(8, 1000)]),
        (1000, vec![(100, 1)], vec![]),
    ]);
    let item = calculate_crafting_profit(
        &[1000],
        &game_data,
        &api_listings,
        None,
        &Default::default(),
    );
    let sources: Vec<_> = item.unwrap().purchases.into_keys().collect();
    assert_eq!(sources, [(12156, crafting::Source::TradingPost)]);

    let prices = HashMap::from([listed(12156, 8)]);
    let prices = SummaryPrices::new(prices, &game_data);
    let estimate = crafting::calculate_estimated_min_crafting_cost(
        12156,
        1,
        &game_data,
        &prices,
        &mut vec![],
        &Default::default(),
    );
    assert_eq!(estimate.unwrap().source, crafting::Source::TradingPost);
}

#[test]
fn an_item_listed_at_its_crafting_cost_is_bought_not_crafted() {
    // 1000 is listed at 100, and crafted from one 1001 at 100
    let game_data = one_ingredient_game_data(1000, 1001, 0);
    let api_listings = api_listings(vec![
        (1001, vec![], vec![(100, 1000)]),
        (1000, vec![(200, 1)], vec![(100, 1)]),
    ]);
    let stop = calculate_crafting_profit(
        &[1000],
        &game_data,
        &api_listings,
        None,
        &Default::default(),
    );
    let cost = Money::from_copper(100);
    let why = format!(
        "Cheaper to buy on the TP ({} each) than to craft ({} each)",
        cost, cost
    );
    assert_eq!(stop, Err(why));
}

#[test]
fn timegated_recipes_are_used_only_when_included() {
    // Lump of Mithrillium, crafted once a day
    let game_data = one_ingredient_game_data(46742, 1001, 0);
    let api_listings = api_listings(vec![
        (1001, vec![], vec![(10, 1000)]),
        (46742, vec![(100, 1)], vec![]),
    ]);
    let prices = SummaryPrices::new(HashMap::from([listed(1001, 10)]), &game_data);
    for include_timegated in [false, true] {
        let opt = CraftingOptions {
            include_timegated,
            ..Default::default()
        };
        let made = calculate_crafting_profit(&[46742], &game_data, &api_listings, None, &opt);
        let estimate = crafting::calculate_estimated_min_crafting_cost(
            46742,
            1,
            &game_data,
            &prices,
            &mut vec![],
            &opt,
        );
        assert_eq!(
            (made.is_ok(), estimate.is_some()),
            (include_timegated, include_timegated)
        );
    }
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
        &mut vec![],
        &Default::default(),
    );
    assert_eq!(estimate.unwrap().cost, Money::from_copper(50));
}
