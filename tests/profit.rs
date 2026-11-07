mod common;

use common::{one_ingredient_game_data, tp_listings_map, MockItem, MockRecipe};

use gw2_arbitrage::{
    api::{ItemListings, RecipeIngredient},
    config::CraftingOptions,
    crafting::{self, CraftedItems, GameData, PurchasedIngredient},
    custom_recipes,
    item::Item,
    money::Money,
    profit::{calculate_crafting_profit, ProfitableItem},
    recipe::{Discipline, Recipe},
};

use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;

#[test]
fn custom_recipe_splits_item_and_currency_ingredients() {
    let custom: custom_recipes::Recipe = serde_json::from_str(
        r#"{
            "name": "Triumphant Hero's Masque",
            "output_item_id": 81476,
            "output_item_count": 1,
            "ingredients": [
                {"count": 175, "type": "Currency", "id": 26},
                {"count": 250, "type": "Item", "id": 71581},
                {"count": 20000, "type": "Currency", "id": 1}
            ],
            "disciplines": ["Merchant"],
            "merchant_data_hash": "xosflg5"
        }"#,
    )
    .unwrap();
    let recipe = Recipe::try_from(custom).unwrap();

    let items: Vec<_> = recipe
        .ingredients
        .iter()
        .map(|i| (i.item_id, i.count))
        .collect();
    let currencies: Vec<_> = recipe
        .currencies
        .iter()
        .map(|c| (c.currency_id, c.count))
        .collect();
    assert_eq!(items, vec![(71581, 250)]);
    assert_eq!(currencies, vec![(26, 175), (1, 20000)]);
}

#[test]
fn additional_recipes_parse() {
    for recipe in custom_recipes::additional_recipes().unwrap() {
        Recipe::try_from(recipe).unwrap();
    }
}

#[test]
fn custom_recipe_alternate_output_id_maps_to_real_item() {
    let custom: custom_recipes::Recipe = serde_json::from_str(
        r#"{
            "name": "Eureka (Desert)",
            "output_item_id": 1000000079562,
            "output_item_count": 1,
            "ingredients": [{"count": 1, "type": "Item", "id": 79570}],
            "disciplines": ["Mystic Forge"]
        }"#,
    )
    .unwrap();
    assert_eq!(custom.output_item_id, 79562);
}

// TODO: in main code, calculate revenue based on selling to buy order volume,
// instead of one at at time. On the upside - this will always be <= true revenue
fn calc_revenue(buys: Vec<(u32, u32)>) -> Money {
    buys.iter()
        .map(|(count, price)| {
            (0..*count)
                .map(|_| Money::from_copper(*price as i32).trading_post_sale_revenue())
                .sum()
        })
        .sum()
}

#[test]
fn calculate_crafting_profit_agony_infusion_unprofitable_test() {
    let data::TestData {
        item_id,
        game_data,
        tp_listings_map,
    } = data::agony_infusions();

    let profitable_item = calculate_crafting_profit(
        item_id,
        &game_data,
        &tp_listings_map,
        None,
        &Default::default(),
    );
    assert!(profitable_item.is_err());
}

#[test]
fn calculate_crafting_profit_agony_infusion_profitable_test() {
    let data::TestData {
        item_id,
        game_data,
        mut tp_listings_map,
    } = data::agony_infusions();
    let (reagent_id, plus_14_id) = (46747, 49437);
    // Adds a reagent at 120 and ten cheap +14s to the base listings
    tp_listings_map.extend(common::tp_listings_map(vec![
        (
            reagent_id,
            vec![(147, 59999), (148, 50790)],
            vec![(179, 33570), (178, 15136), (120, 1)],
        ),
        (
            plus_14_id,
            vec![(1900958, 1), (1900960, 4)],
            vec![
                (2489189, 1),
                (2489188, 1),
                (1100000, 7),
                (1000000, 2),
                (800000, 1),
            ],
        ),
    ]));

    let mut purchased_ingredients = HashMap::new();
    let profitable_item = calculate_crafting_profit(
        item_id,
        &game_data,
        &tp_listings_map,
        Some(&mut purchased_ingredients),
        &Default::default(),
    );

    let mut purchased_ingredients: Vec<_> = purchased_ingredients.into_iter().collect();
    purchased_ingredients.sort_by_key(|(key, _)| *key);
    let bought = |count, min, max, total| PurchasedIngredient {
        count,
        min_price: Money::from_copper(min),
        max_price: Money::from_copper(max),
        total_cost: Money::from_copper(total),
    };
    use crafting::Source::{TradingPost, Vendor};
    assert_eq!(
        purchased_ingredients,
        vec![
            ((reagent_id, TradingPost), bought(1, 120, 120, 120)),
            // the vendor's minimum purchase
            ((reagent_id, Vendor), bought(10, 0, 0, 0)),
            (
                (plus_14_id, TradingPost),
                bought(8, 800000, 1100000, 8300000)
            ),
        ]
    );

    // One reagent is bought from the TP; the vendor's 149.6 beats the next listing at 178, so the
    // other 5 come from a pack of 10, and the 5 left over are sold into buy orders at 148
    // 1496 / 10, favoring first
    let vendor_reagent = [150, 150, 149, 150, 149, 150, 150, 149, 150, 149];
    let crafting_cost = Money::from_copper(800000 + 2 * 1000000 + 5 * 1100000 + 120)
        + Money::from_copper(vendor_reagent.iter().sum());
    let leftover_revenue = Money::from_copper(148).trading_post_sale_revenue() * 5;
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            id: item_id,
            crafting_cost,
            count: 2,
            profit: Money::from_copper(7982220 + 7982200).trading_post_sale_revenue()
                + leftover_revenue
                - crafting_cost,
            max_sell: Money::from_copper(7982220),
            min_sell: Money::from_copper(7982200),
            // the second +16: four +14s and three vendor reagents
            breakeven: (Money::from_copper(1100000 * 4)
                + Money::from_copper(vendor_reagent[2..5].iter().sum()))
            .trading_post_listing_price(),
            crafting_steps: 6,
            crafting_seconds: 2,
            // 2 TP buys, 3 ingredients, output, 2 sales
            market_seconds: 10 + 18 + 3 + 10,
            crafted_items: CraftedItems {
                crafted: HashMap::from([(plus_14_id + 1, 4), (item_id, 2)]),
                leftovers: HashMap::from([(
                    reagent_id,
                    (
                        5,
                        Money::from_copper(vendor_reagent[5..].iter().sum()),
                        Vendor
                    )
                )]),
            },
        })
    );
}

#[test]
fn calculate_crafting_profit_with_output_item_count_test() {
    let item_id = 1236;

    let mut items_map = HashMap::new();
    items_map.insert(1234, Item::mock(1234, "Ingredient 2", 0));
    items_map.insert(1235, Item::mock(1235, "Ingredient 1", 0));
    items_map.insert(item_id, Item::mock(item_id, "Main item", 0));

    let mut recipes_map = HashMap::new();
    recipes_map.insert(
        item_id,
        Recipe::mock(
            7852,
            item_id,
            1,
            [],
            &[
                RecipeIngredient {
                    item_id: 1234,
                    count: 2,
                },
                RecipeIngredient {
                    item_id: 1235,
                    count: 1,
                },
            ],
            true,
        ),
    );
    let mut game_data = GameData {
        recipes: recipes_map,
        items: items_map,
    };

    let tp_listings_map = tp_listings_map(vec![
        (1234, vec![], vec![(94, 50), (92, 33), (90, 1)]),
        (1235, vec![], vec![(59, 50), (45, 33), (43, 1)]),
        (item_id, vec![(198, 47), (199, 50), (200, 1)], vec![]),
    ]);

    // A batch of 99 sells into the 98 buy orders, and the last one is left over
    game_data
        .recipes
        .get_mut(&item_id)
        .unwrap()
        .output_item_count = 99;
    let profitable_item = calculate_crafting_profit(
        item_id,
        &game_data,
        &tp_listings_map,
        None,
        &Default::default(),
    );
    let copper_cost: i32 = 43 + 90 + 92;
    let crafting_cost = Money::from_copper(copper_cost);
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            id: item_id,
            crafting_cost,
            count: 98,
            profit: calc_revenue(vec![(47, 198), (50, 199), (1, 200)]) - crafting_cost,
            max_sell: Money::from_copper(200),
            min_sell: Money::from_copper(198),
            breakeven: profitable_item.as_ref().unwrap().breakeven,
            crafting_steps: 1,
            crafting_seconds: 1,
            // 2 ingredients, output, 3 price levels
            market_seconds: 10 + 12 + 3 + 15,
            crafted_items: CraftedItems {
                crafted: HashMap::from([(item_id, 99)]),
                leftovers: HashMap::from([(
                    item_id,
                    (
                        1,
                        Money::from_copper(copper_cost / 99),
                        crafting::Source::Crafting
                    )
                )]),
            },
        })
    );

    game_data
        .recipes
        .get_mut(&item_id)
        .unwrap()
        .output_item_count = 98;
    let profitable_item = calculate_crafting_profit(
        item_id,
        &game_data,
        &tp_listings_map,
        None,
        &Default::default(),
    );
    let crafting_cost = Money::from_copper(43 + 90 + 92);
    let mut crafted = HashMap::new();
    crafted.insert(item_id, 98);
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            id: item_id,
            crafting_cost,
            count: 98,
            profit: calc_revenue(vec![(47, 198), (50, 199), (1, 200)]) - crafting_cost,
            max_sell: Money::from_copper(200),
            min_sell: Money::from_copper(198),
            breakeven: profitable_item.as_ref().unwrap().breakeven, // inexact, test below
            crafting_steps: 1,
            crafting_seconds: 1,
            // 2 ingredients, output, 3 price levels
            market_seconds: 10 + 12 + 3 + 15,
            crafted_items: CraftedItems {
                crafted,
                leftovers: Default::default(),
            },
        })
    );
    assert_eq!(
        profitable_item
            .as_ref()
            .unwrap()
            .breakeven
            .to_copper_value(),
        4 // ((43 + 90 + 92) / 98) / (85/100); min of 2 copper added, rounded down since we front-load cost
    );

    game_data
        .recipes
        .get_mut(&item_id)
        .unwrap()
        .output_item_count = 3;
    let profitable_item = calculate_crafting_profit(
        item_id,
        &game_data,
        &tp_listings_map,
        None,
        &Default::default(),
    );
    // 33 batches: the last sells 2 into the last buy orders, and 1 is left over
    let crafting_cost = Money::from_copper(43 + 45 * 32 + 90 + 92 * 33 + 94 * 32);
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            id: item_id,
            crafting_cost,
            count: 98,
            profit: calc_revenue(vec![(47, 198), (50, 199), (1, 200)]) - crafting_cost,
            max_sell: Money::from_copper(200),
            min_sell: Money::from_copper(198),
            // ((2*94 + 45) / 3) / (85/100)
            breakeven: Money::from_copper(92),
            crafting_steps: 33,
            crafting_seconds: 1,
            // 2 ingredients, output, 3 price levels
            market_seconds: 10 + 12 + 3 + 15,
            crafted_items: CraftedItems {
                crafted: HashMap::from([(item_id, 99)]),
                leftovers: HashMap::from([(
                    item_id,
                    (
                        1,
                        Money::from_copper((2 * 94 + 45) / 3),
                        crafting::Source::Crafting
                    )
                )]),
            },
        })
    );
}

#[test]
fn calculate_crafting_profit_unknown_recipe_test() {
    struct TestItem {
        name: String,
        id: u32,
        recipe_id: Option<u32>,
        ingredients: Vec<u32>,
    }

    let ingredient_1 = TestItem {
        name: "Purchasable ingredient 1".to_string(),
        id: 12356,
        recipe_id: None,
        ingredients: vec![],
    };
    let ingredient_2 = TestItem {
        name: "Purchasable ingredient 2".to_string(),
        id: 12357,
        recipe_id: None,
        ingredients: vec![],
    };

    let ingredient_of_ingredient_3 = TestItem {
        name: "Purchasable ingredient 3 - sub-ingredient".to_string(),
        id: 12359,
        recipe_id: None,
        ingredients: vec![],
    };
    let ingredient_3 = TestItem {
        name: "Purchasable ingredient 3".to_string(),
        id: 12358,
        recipe_id: Some(7856),
        ingredients: vec![ingredient_of_ingredient_3.id],
    };

    let crafted_ingredient_with_known_recipe = TestItem {
        name: "Crafted ingredient with known recipe".to_string(),
        id: 1233,
        recipe_id: Some(7853),
        ingredients: vec![ingredient_1.id],
    };
    let crafted_ingredient_with_unknown_recipe = TestItem {
        name: "Crafted ingredient with unknown recipe".to_string(),
        id: 1234,
        recipe_id: Some(7854),
        ingredients: vec![ingredient_2.id],
    };
    let crafted_ingredient_cheaper_on_trading_post = TestItem {
        name: "Crafted ingredient which is cheaper to buy on trading post".to_string(),
        id: 1235,
        recipe_id: Some(7855),
        ingredients: vec![ingredient_3.id],
    };
    let main_item = TestItem {
        name: "Main item".to_string(),
        id: 1232,
        recipe_id: Some(7852),
        ingredients: vec![
            crafted_ingredient_with_known_recipe.id,
            crafted_ingredient_with_unknown_recipe.id,
            crafted_ingredient_cheaper_on_trading_post.id,
        ],
    };

    let mut known_recipes = HashSet::new();
    known_recipes.insert(crafted_ingredient_with_known_recipe.recipe_id.unwrap());

    let mut expected_unknown_recipes = HashSet::new();
    expected_unknown_recipes.insert(main_item.recipe_id.unwrap());
    expected_unknown_recipes.insert(crafted_ingredient_with_unknown_recipe.recipe_id.unwrap());

    let mut expected_purchased_ingredients = HashSet::new();
    expected_purchased_ingredients.insert(crafted_ingredient_cheaper_on_trading_post.id);
    expected_purchased_ingredients.insert(ingredient_1.id);
    expected_purchased_ingredients.insert(ingredient_2.id);

    let mut items_map = HashMap::new();
    let mut recipes_map = HashMap::new();
    let mut tp_listings = vec![];
    for item in [
        &main_item,
        &crafted_ingredient_with_known_recipe,
        &crafted_ingredient_with_unknown_recipe,
        &crafted_ingredient_cheaper_on_trading_post,
        &ingredient_1,
        &ingredient_2,
        &ingredient_3,
        &ingredient_of_ingredient_3,
    ] {
        items_map.insert(item.id, Item::mock(item.id, &item.name, 0));

        if let Some(recipe_id) = item.recipe_id {
            recipes_map.insert(
                item.id,
                Recipe::mock(
                    recipe_id,
                    item.id,
                    1,
                    [],
                    &item
                        .ingredients
                        .iter()
                        .map(|&ingredient_id| RecipeIngredient {
                            item_id: ingredient_id,
                            count: 1,
                        })
                        .collect::<Vec<_>>(),
                    false,
                ),
            );
        }

        // prices chosen such that ingredient 3 will first be marked as craftable
        // because crafting it is cheaper than buying it from the tp, but it will then
        // have to be discarded because the parent item of ingredient 3 is cheaper to
        // buy from the tp than to craft.
        let sells = if item.id == ingredient_of_ingredient_3.id {
            vec![(2, 100)]
        } else if item.id == ingredient_1.id
            || item.id == ingredient_2.id
            || item.id == ingredient_3.id
        {
            vec![(3, 100)]
        } else if item.id == crafted_ingredient_cheaper_on_trading_post.id {
            vec![(1, 100)]
        } else {
            vec![]
        };
        let buys = if item.id == main_item.id {
            vec![(198, 47), (199, 50), (200, 1)]
        } else {
            vec![]
        };
        tp_listings.push((item.id, buys, sells));
    }
    let game_data = GameData {
        recipes: recipes_map,
        items: items_map,
    };

    let mut purchased_ingredients = HashMap::new();
    let profitable_item = calculate_crafting_profit(
        main_item.id,
        &game_data,
        &tp_listings_map(tp_listings),
        Some(&mut purchased_ingredients),
        &Default::default(),
    );

    assert!(profitable_item.is_ok());
    assert_eq!(
        purchased_ingredients
            .into_iter()
            .map(|((item_id, _), _)| item_id)
            .collect::<HashSet<u32>>(),
        expected_purchased_ingredients
    );
    assert_eq!(
        profitable_item
            .unwrap()
            .crafted_items
            .unknown_recipes(&game_data.recipes, &known_recipes),
        expected_unknown_recipes
    );
}

#[test]
fn calculate_crafting_profit_with_subitem_leftovers() {
    let mut items_map = HashMap::new();
    items_map.insert(1000, Item::mock(1000, "Output Item", 0));
    items_map.insert(2000, Item::mock(2000, "Ingredient 1", 0));
    items_map.insert(2100, Item::mock(2010, "Ingredient 2", 0));
    items_map.insert(2110, Item::mock(2010, "Sub-ingredient 1", 0));

    let mut recipes_map = HashMap::new();
    recipes_map.insert(
        1000,
        Recipe::mock(
            300,
            1000,
            1,
            [],
            &[
                RecipeIngredient {
                    item_id: 2000,
                    count: 2,
                },
                RecipeIngredient {
                    item_id: 2100,
                    count: 1,
                },
            ],
            true,
        ),
    );
    recipes_map.insert(
        2100,
        Recipe::mock(
            301,
            2100,
            5,
            [],
            &[RecipeIngredient {
                item_id: 2110,
                count: 1,
            }],
            true,
        ),
    );

    let tp_listings_map = tp_listings_map(vec![
        (1000, vec![(100, 100), (155, 50), (200, 1)], vec![]),
        (2000, vec![], vec![(50, 100), (30, 10), (25, 1)]), // needs 2
        (2100, vec![(100, 30)], vec![(31, 100), (21, 11), (15, 1)]),
        (2110, vec![], vec![(150, 20), (125, 6), (100, 1)]), // 30, 25, 20 per crafted 2100
    ]);
    let game_data = GameData {
        recipes: recipes_map,
        items: items_map,
    };

    let mut purchased_ingredients = HashMap::new();
    let profitable_item = calculate_crafting_profit(
        1000,
        &game_data,
        &tp_listings_map,
        Some(&mut purchased_ingredients),
        &Default::default(),
    );
    let crafting_cost = Money::from_copper(
        (25 + 30 + 15) // 1
        + (30 * 8 + 4 * 20) // +4 = 5
        + (30 + 50 + 20) // +1 = 6
        + (50 * 2 * 11 + 21 * 11) // +11 = 17
        + (50 * 2 * 30 + 25 * 5 * 6) // +30 = 47
        + (50 * 2 * 4 + 30 * 4) // +4 = 51
        + 30, // the leftover, sold for its cost
    );
    // One leftover, not profitable at 100
    let mut leftovers: HashMap<u32, (u32, Money, crafting::Source)> = HashMap::new();
    leftovers.insert(
        2100,
        (1, Money::from_copper(30), crafting::Source::Crafting),
    );
    let mut crafted = HashMap::new();
    crafted.insert(1000, 51);
    crafted.insert(2100, 40);
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            id: 1000,
            crafting_cost,
            count: 51,
            profit: calc_revenue(vec![(50, 155), (1, 200)]) + Money::from_copper(30)
                - crafting_cost,
            max_sell: Money::from_copper(200),
            min_sell: Money::from_copper(155),
            // (50 * 2 + 30) / (85/100)
            breakeven: Money::from_copper(153),
            crafting_steps: 59,
            crafting_seconds: 2,
            // 3 ingredients, output, 2 price levels
            market_seconds: 15 + 18 + 3 + 10,
            crafted_items: CraftedItems { crafted, leftovers },
        })
    );
}

/// Profit of item 1000, made from one of ingredient 1001, without crafting time
fn one_ingredient_profit(
    ingredient_sells: Vec<(u32, u32)>,
    buys: Vec<(u32, u32)>,
    opt: &CraftingOptions,
) -> Option<ProfitableItem> {
    let game_data = one_ingredient_game_data(1000, 1001, 0);
    let tp_listings_map =
        tp_listings_map(vec![(1001, vec![], ingredient_sells), (1000, buys, vec![])]);
    calculate_crafting_profit(1000, &game_data, &tp_listings_map, None, opt).ok()
}

#[test]
fn calculate_crafting_profit_splits_market_actions_at_stack_size() {
    let profitable_item = one_ingredient_profit(
        vec![(1, 261)],
        vec![(999, 10), (1000, 251)],
        &Default::default(),
    )
    .unwrap();

    // Buys of 250 and 11 of one ingredient; sales of 250 and 1 at one price and 10 at the next
    assert_eq!(profitable_item.count, 261);
    // 2 buys, 1 ingredient, output, 3 sales less 1s of crafting, while the 11 left sell after
    assert_eq!(profitable_item.market_seconds, 10 + 6 + 3 + 14);
}

#[test]
fn calculate_crafting_profit_min_per_hour_stops_at_best_count() {
    let at_rate = |gold| {
        one_ingredient_profit(
            vec![(1, 100)],
            vec![(1000, 1), (12, 1), (10, 1)],
            &CraftingOptions {
                min_per_hour: Some(gold),
                ..Default::default()
            },
        )
    };

    // At 1g / hour, 5 seconds is worth about 14c, more than the 2nd and 3rd sales earn
    let profitable_item = at_rate(1).unwrap();
    assert_eq!(profitable_item.count, 1);
    assert_eq!(profitable_item.market_seconds, 5 + 6 + 3 + 5);

    // At 100g / hour, even the first sale isn't worth the time
    assert!(at_rate(100).is_none());
}

/// Output id, output count, and (ingredient id, count)s
type MockRecipeSpec<'a> = (u32, u32, &'a [(u32, u32)]);

/// Mock items and recipes; real item ids where `vendor_cost` matches on id
fn mock_game_data(items: &[(u32, &str, u32)], recipes: &[MockRecipeSpec]) -> GameData {
    let items = items
        .iter()
        .map(|&(id, name, value)| (id, Item::mock(id, name, value)));
    let recipe = |&(id, count, ingredients): &MockRecipeSpec| {
        let ingredients: Vec<RecipeIngredient> = ingredients
            .iter()
            .map(|&(item_id, count)| RecipeIngredient { item_id, count })
            .collect();
        (id, Recipe::mock(id, id, count, [], &ingredients, true))
    };
    GameData {
        recipes: recipes.iter().map(recipe).collect(),
        items: items.collect(),
    }
}

/// How many of `ingredient_id` are crafted for `count` of the item, valued high
fn crafted_ingredients(
    item_id: u32,
    count: u32,
    ingredient_id: u32,
    game_data: &GameData,
    tp_listings_map: &HashMap<u32, ItemListings>,
) -> u32 {
    let opt = CraftingOptions {
        value: Some(100000),
        count: Some(count),
        ..Default::default()
    };
    let profitable_item =
        calculate_crafting_profit(item_id, game_data, tp_listings_map, None, &opt).unwrap();
    assert_eq!(profitable_item.count, count);
    let crafted = profitable_item.crafted_items.crafted.get(&ingredient_id);
    crafted.copied().unwrap_or(0)
}

/// Bowls of Baker's Dry Ingredients crafted for 120 Homemade Campfire Treats (x12, 8 bowls
/// each); a bowl's 4 ingredients are sold by vendors in 10s at 8c, so it costs 32 to craft
fn campfire_treat_bowls(bowl_price: u32) -> u32 {
    let game_data = mock_game_data(
        &[
            (95421, "Homemade Campfire Treat", 0),
            (12169, "Bowl of Baker's Dry Ingredients", 3),
            (12155, "Bag of Sugar", 1),
            (91702, "Pile of Powdered Gelatin Mix", 48),
            (12229, "Chocolate Bar", 1),
            (12136, "Bag of Flour", 1),
            (12151, "Packet of Baking Powder", 1),
            (12153, "Packet of Salt", 1),
        ],
        &[
            (
                95421,
                12,
                &[(12155, 12), (91702, 3), (12229, 6), (12169, 8)],
            ),
            (12169, 1, &[(12136, 1), (12151, 1), (12153, 1), (12155, 1)]),
        ],
    );
    let tp_listings_map = tp_listings_map(vec![
        (12169, vec![], vec![(bowl_price, 10000)]),
        (12229, vec![], vec![(10, 10000)]),
    ]);
    crafted_ingredients(95421, 120, 12169, &game_data, &tp_listings_map)
}

/// Silk Patches (x5) crafted for 20 Carrion Embroidered Silk Insignias (1 patch each); a
/// patch's 25 Spools of Silk Thread are sold by vendors in 10s at 48c
fn silk_insignia_patches(patch_price: u32) -> u32 {
    let game_data = mock_game_data(
        &[
            (19859, "Carrion Embroidered Silk Insignia", 32),
            (72194, "Silk Patch", 7),
            (24341, "Large Bone", 11),
            (19735, "Cured Thick Leather Square", 7),
            (19747, "Bolt of Silk", 7),
            (19791, "Spool of Silk Thread", 6),
        ],
        &[
            (19859, 1, &[(72194, 1), (24341, 8)]),
            (72194, 5, &[(19735, 10), (19747, 4), (19791, 25)]),
        ],
    );
    // A batch of 5 patches is 10 * 100 + 4 * 200 + 25 * 48 = 3000, 600 each
    let tp_listings_map = tp_listings_map(vec![
        (72194, vec![], vec![(patch_price, 10000)]),
        (24341, vec![], vec![(10, 10000)]),
        (19735, vec![], vec![(100, 10000)]),
        (19747, vec![], vec![(200, 10000)]),
    ]);
    crafted_ingredients(19859, 20, 72194, &game_data, &tp_listings_map)
}

#[test]
fn calculate_crafting_profit_buys_the_own_output_a_promotion_uses() {
    // 250 Copper Ore, 1 Iron Ore and 5 dust make 88 Iron Ore
    let game_data = mock_game_data(
        &[(1, "Iron Ore", 0), (2, "Copper Ore", 0), (3, "Dust", 0)],
        &[(1, 88, &[(2, 250), (1, 1), (3, 5)])],
    );
    let tp_listings_map = tp_listings_map(vec![
        (1, vec![], vec![(100, 1000)]),
        (2, vec![], vec![(1, 1000)]),
        (3, vec![], vec![(1, 1000)]),
    ]);
    let crafted = crafted_ingredients(1, 88, 1, &game_data, &tp_listings_map);
    assert_eq!(crafted, 88);
}

#[test]
fn calculate_crafting_profit_makes_the_count_asked_for_from_part_of_a_batch() {
    let game_data = mock_game_data(
        &[(1, "Batch of 3", 0), (2, "Part", 0)],
        &[(1, 3, &[(2, 1)])],
    );
    let tp_listings_map = tp_listings_map(vec![(2, vec![], vec![(10, 1000)])]);
    // 4 batches for 10, with 2 left over
    let crafted = crafted_ingredients(1, 10, 1, &game_data, &tp_listings_map);
    assert_eq!(crafted, 12);
}

#[test]
fn calculate_crafting_profit_buys_cheap_listings_and_crafts_the_rest() {
    let game_data = mock_game_data(
        &[(1, "Parent", 0), (2, "Part", 0), (3, "Material", 0)],
        &[(1, 1, &[(2, 10)]), (2, 1, &[(3, 1)])],
    );
    // A part costs 50 to craft; only 4 are listed below that
    let tp_listings_map = tp_listings_map(vec![
        (2, vec![], vec![(30, 4), (100, 1000)]),
        (3, vec![], vec![(50, 1000)]),
    ]);
    let crafted = crafted_ingredients(1, 1, 2, &game_data, &tp_listings_map);
    assert_eq!(crafted, 6);
}

#[test]
fn calculate_crafting_profit_buys_sub_ingredient_at_or_below_crafting_cost() {
    assert_eq!(campfire_treat_bowls(32), 0);
    assert_eq!(campfire_treat_bowls(31), 0);
    assert_eq!(silk_insignia_patches(600), 0);
    assert_eq!(silk_insignia_patches(599), 0);
}

#[test]
fn calculate_crafting_profit_keeps_crafting_cheaper_sub_ingredient_with_vendor_leftovers() {
    assert_eq!(campfire_treat_bowls(33), 80);
    assert_eq!(silk_insignia_patches(601), 20);
}

mod data {
    use super::*;

    pub struct TestData {
        pub item_id: u32,
        pub game_data: GameData,
        pub tp_listings_map: HashMap<u32, ItemListings>,
    }

    /// Recipe with very large number of ingredients but low tp liquidity
    pub fn agony_infusions() -> TestData {
        let item_id = 49439;

        let mut items_map = HashMap::new();
        items_map.insert(46747, Item::mock(46747, "Thermocatalytic Reagent", 80));
        for (id, bonus) in (49424..=49439).zip(1..=16) {
            items_map.insert(
                id,
                Item::mock(id, format!("+{} Agony Infusion", bonus).as_str(), 330),
            );
        }

        let mut recipes_map = HashMap::new();
        for (id, recipe_id) in (49425..=49439).zip(7851..=7865) {
            recipes_map.insert(
                id,
                Recipe::mock(
                    recipe_id,
                    id,
                    1,
                    [Discipline::Artificer],
                    &[
                        RecipeIngredient {
                            item_id: id - 1,
                            count: 2,
                        },
                        RecipeIngredient {
                            item_id: 46747,
                            count: 1,
                        },
                    ],
                    true,
                ),
            );
        }

        // Item id, buy orders and sell listings, each as (unit price, quantity)
        let tp_listings_map = tp_listings_map(vec![
            (
                46747, // Thermocatalytic Reagent
                vec![(147, 59999), (148, 50790)],
                vec![(179, 33570), (178, 15136)],
            ),
            (
                49424, // +1 Agony Infusion
                vec![(73, 8874), (74, 71424)],
                vec![(81, 553), (80, 112)],
            ),
            (
                49425, // +2 Agony Infusion
                vec![(304, 194), (305, 1000)],
                vec![(452, 1), (451, 1152)],
            ),
            (
                49426, // +3 Agony Infusion
                vec![(749, 213), (751, 355)],
                vec![(775, 8), (774, 5)],
            ),
            (
                49427, // +4 Agony Infusion
                vec![(1937, 319), (1950, 1193)],
                vec![(2290, 16), (2100, 12)],
            ),
            (
                49428, // +5 Agony Infusion
                vec![(3387, 242), (3500, 10)],
                vec![(4495, 6), (4494, 1)],
            ),
            (
                49429, // +6 Agony Infusion
                vec![(6500, 16), (6600, 3)],
                vec![(7082, 1), (6333, 2)],
            ),
            (
                49430, // +7 Agony Infusion
                vec![(14469, 8), (14475, 27)],
                vec![(17000, 1), (16996, 1)],
            ),
            (
                49431, // +8 Agony Infusion
                vec![(30700, 1), (30707, 3)],
                vec![(35900, 11), (35897, 4)],
            ),
            (
                49432, // +9 Agony Infusion
                vec![(58033, 87), (58034, 45)],
                vec![(69000, 1), (68999, 5)],
            ),
            (
                49433, // +10 Agony Infusion
                vec![(115306, 3), (115307, 2)],
                vec![(141500, 3), (141300, 2)],
            ),
            (
                49434, // +11 Agony Infusion
                vec![(235902, 2), (235903, 3)],
                vec![(298392, 1), (298390, 1)],
            ),
            (
                49435, // +12 Agony Infusion
                vec![(454981, 1), (454982, 2)],
                vec![(585500, 1), (585499, 1)],
            ),
            (
                49436, // +13 Agony Infusion
                vec![(944117, 1), (944118, 1)],
                vec![(1239994, 1), (1239990, 1)],
            ),
            (
                49437, // +14 Agony Infusion
                vec![(1900958, 1), (1900960, 4)],
                vec![(2489189, 1), (2489188, 1)],
            ),
            (
                49438, // +15 Agony Infusion
                vec![(3509999, 1), (3510000, 1)],
                vec![(4749997, 1), (4499999, 1)],
            ),
            (
                49439, // +16 Agony Infusion
                vec![(7982200, 1), (7982220, 1)],
                vec![(9499998, 1), (9499997, 1)],
            ),
        ]);

        TestData {
            item_id,
            game_data: GameData {
                recipes: recipes_map,
                items: items_map,
            },
            tp_listings_map,
        }
    }
}
