mod common;

use common::{api_listings, one_ingredient_game_data, MockItem, MockRecipe};

use gw2_arbitrage::{
    api::{ItemListings, RecipeIngredient},
    config::CraftingOptions,
    crafting::{self, CraftedItems, GameData, PurchasedIngredient},
    custom_recipes,
    item::Item,
    money::Money,
    profit::{
        calculate_capped_profit, calculate_crafting_profit, leftover_sale, ProfitableItem, Sale,
    },
    recipe::{Discipline, Recipe},
    trading_post,
};

use std::collections::{BTreeMap, HashMap, HashSet};
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

/// Each (count, price) is one transaction, its fees charged on its total
fn calc_revenue(buys: Vec<(u32, u32)>) -> Money {
    let totals = buys.iter().map(|(count, price)| count * price);
    let totals = totals.map(|total| Money::from_copper(total as i32));
    totals.map(Money::trading_post_sale_revenue).sum()
}

/// A shopping list entry: how many, the lowest and highest unit price paid, and the total
fn bought(count: u32, min: i32, max: i32, total: i32) -> PurchasedIngredient {
    PurchasedIngredient {
        count,
        min_price: Money::from_copper(min),
        max_price: Money::from_copper(max),
        total_cost: Money::from_copper(total),
    }
}

#[test]
fn calculate_crafting_profit_agony_infusion_unprofitable_test() {
    let data::TestData {
        item_id,
        game_data,
        api_listings,
    } = data::agony_infusions();

    let profitable_item = calculate_crafting_profit(
        &[item_id],
        &game_data,
        &api_listings,
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
        mut api_listings,
    } = data::agony_infusions();
    let (reagent_id, plus_14_id) = (46747, 49437);
    // Adds a reagent at 120 and ten cheap +14s to the base listings
    api_listings.extend(common::api_listings(vec![
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

    let profitable_item = calculate_crafting_profit(
        &[item_id],
        &game_data,
        &api_listings,
        None,
        &Default::default(),
    );

    use crafting::Source::{TradingPost, Vendor};
    // One reagent is bought from the TP; the vendor's 149.6 beats the next listing at 178, so the
    // other 5 come from a pack of 10, and the 5 left over are sold into buy orders at 148
    // 1496 / 10, favoring first
    let vendor_reagent = [150, 150, 149, 150, 149, 150, 150, 149, 150, 149];
    let crafting_cost = Money::from_copper(800000 + 2 * 1000000 + 5 * 1100000 + 120)
        + Money::from_copper(vendor_reagent.iter().sum());
    let leftover_cost = Money::from_copper(vendor_reagent[5..].iter().sum());
    let revenue = calc_revenue(vec![(1, 7982220), (1, 7982200)]);
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            sales: vec![Sale {
                id: item_id,
                count: 2,
                max_sell: Money::from_copper(7982220),
                min_sell: Money::from_copper(7982200),
                // the second +16: four +14s and three vendor reagents
                breakeven: (Money::from_copper(1100000 * 4)
                    + Money::from_copper(vendor_reagent[2..5].iter().sum()))
                .trading_post_listing_price(),
                profit: revenue - (crafting_cost - leftover_cost),
                stop: String::new(),
            }],
            crafting_cost,
            profit: revenue + calc_revenue(vec![(5, 148)]) - crafting_cost,
            // a transaction per sale: 5% listing fees, and 10% exchange fees from the payout
            money_required: crafting_cost + Money::from_copper(399111 + 399110),
            payout: Some(Money::from_copper(7982220 - 798222 + 7982200 - 798220)),
            // 5 into buy orders at 148, 629 after fees; the 178 left after the 120 bought lists
            // for 151 after fees; 748 / 5 marked up for fees breaks even
            leftover_sales: HashMap::from([(
                reagent_id,
                (
                    5,
                    Some(Money::from_copper(126)),
                    Some(Money::from_copper(151)),
                    Money::from_copper(177),
                ),
            )]),
            crafting_steps: 6,
            crafting_seconds: 2,
            // 2 TP buys, 3 ingredients, output, 2 sales
            market_seconds: 10 + 18 + 3 + 10,
            crafted_items: CraftedItems {
                crafted: HashMap::from([((plus_14_id + 1, 0), 4), ((item_id, 0), 2)]),
                leftovers: HashMap::from([(reagent_id, (5, leftover_cost, Vendor))]),
                lots: HashMap::from([(reagent_id, (Money::from_copper(1496), 10))]),
                ..Default::default()
            },
            purchases: HashMap::from([
                ((reagent_id, TradingPost), bought(1, 120, 120, 120)),
                // the vendor's minimum purchase
                ((reagent_id, Vendor), bought(10, 0, 0, 1496)),
                (
                    (plus_14_id, TradingPost),
                    bought(8, 800000, 1100000, 8300000)
                ),
            ]),
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
        vec![Recipe::mock(
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
        )],
    );
    let mut game_data = GameData {
        recipes: recipes_map,
        items: items_map,
        unlocked_by: HashMap::new(),
    };

    let api_listings = api_listings(vec![
        (1234, vec![], vec![(94, 50), (92, 33), (90, 1)]),
        (1235, vec![], vec![(59, 50), (45, 33), (43, 1)]),
        (item_id, vec![(198, 47), (199, 50), (200, 1)], vec![]),
    ]);
    // every case sells into all 98 buy orders, a transaction per price
    let revenue = calc_revenue(vec![(47, 198), (50, 199), (1, 200)]);
    // those transactions are 9306, 9950 and 200: 5% listing fees from the wallet, halves rounded
    // up, and 10% exchange fees from the payout
    let listing_fees = Money::from_copper(465 + 498 + 10);
    let payout = Some(Money::from_copper(9306 - 931 + 9950 - 995 + 200 - 20));
    use crafting::Source::TradingPost;
    // the cheapest 2 of 1234 and 1 of 1235
    let one_batch = || {
        HashMap::from([
            ((1234, TradingPost), bought(2, 90, 92, 90 + 92)),
            ((1235, TradingPost), bought(1, 43, 43, 43)),
        ])
    };

    // A batch of 99 sells into the 98 buy orders, and the last one is left over
    game_data.recipes.get_mut(&item_id).unwrap()[0].output_item_count = 99;
    let profitable_item = calculate_crafting_profit(
        &[item_id],
        &game_data,
        &api_listings,
        None,
        &Default::default(),
    );
    let copper_cost: i32 = 43 + 90 + 92;
    let crafting_cost = Money::from_copper(copper_cost);
    // the leftover has no buy orders left to sell into
    let leftover_cost = Money::from_copper(copper_cost / 99);
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            sales: vec![Sale {
                id: item_id,
                count: 98,
                max_sell: Money::from_copper(200),
                min_sell: Money::from_copper(198),
                breakeven: profitable_item.as_ref().unwrap().sales[0].breakeven,
                profit: revenue - (crafting_cost - leftover_cost),
                stop: String::new(),
            }],
            crafting_cost,
            profit: revenue - crafting_cost,
            money_required: crafting_cost + listing_fees,
            payout,
            // nothing to sell into or list under; 2 marked up for fees, at least 2 more
            leftover_sales: HashMap::from([(item_id, (0, None, None, Money::from_copper(4)))]),
            crafting_steps: 1,
            crafting_seconds: 1,
            // 2 ingredients, output, 3 price levels
            market_seconds: 10 + 12 + 3 + 15,
            crafted_items: CraftedItems {
                crafted: HashMap::from([((item_id, 0), 99)]),
                leftovers: HashMap::from([(
                    item_id,
                    (1, leftover_cost, crafting::Source::Crafting)
                )]),
                lots: HashMap::from([(item_id, (crafting_cost, 99))]),
                ..Default::default()
            },
            purchases: one_batch(),
        })
    );

    game_data.recipes.get_mut(&item_id).unwrap()[0].output_item_count = 98;
    let profitable_item = calculate_crafting_profit(
        &[item_id],
        &game_data,
        &api_listings,
        None,
        &Default::default(),
    );
    let crafting_cost = Money::from_copper(43 + 90 + 92);
    let mut crafted = HashMap::new();
    crafted.insert((item_id, 0), 98);
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            sales: vec![Sale {
                id: item_id,
                count: 98,
                max_sell: Money::from_copper(200),
                min_sell: Money::from_copper(198),
                // ((43 + 90 + 92) / 98) / (85/100); min of 2 copper added, rounded down since we
                // front-load cost
                breakeven: Money::from_copper(4),
                profit: revenue - crafting_cost,
                stop: String::new(),
            }],
            crafting_cost,
            profit: revenue - crafting_cost,
            money_required: crafting_cost + listing_fees,
            payout,
            leftover_sales: HashMap::new(),
            crafting_steps: 1,
            crafting_seconds: 1,
            // 2 ingredients, output, 3 price levels
            market_seconds: 10 + 12 + 3 + 15,
            crafted_items: CraftedItems {
                crafted,
                ..Default::default()
            },
            purchases: one_batch(),
        })
    );

    game_data.recipes.get_mut(&item_id).unwrap()[0].output_item_count = 3;
    let profitable_item = calculate_crafting_profit(
        &[item_id],
        &game_data,
        &api_listings,
        None,
        &Default::default(),
    );
    // 33 batches: the last sells 2 into the last buy orders, and 1 is left over
    let crafting_cost = Money::from_copper(43 + 45 * 32 + 90 + 92 * 33 + 94 * 32);
    let leftover_cost = Money::from_copper((2 * 94 + 45) / 3);
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            sales: vec![Sale {
                id: item_id,
                count: 98,
                max_sell: Money::from_copper(200),
                min_sell: Money::from_copper(198),
                // ((2*94 + 45) / 3) / (85/100)
                breakeven: Money::from_copper(92),
                profit: revenue - (crafting_cost - leftover_cost),
                stop: String::new(),
            }],
            crafting_cost,
            profit: revenue - crafting_cost,
            money_required: crafting_cost + listing_fees,
            payout,
            // 77 marked up for fees
            leftover_sales: HashMap::from([(item_id, (0, None, None, Money::from_copper(91)))]),
            crafting_steps: 33,
            crafting_seconds: 1,
            // 2 ingredients, output, 3 price levels
            market_seconds: 10 + 12 + 3 + 15,
            crafted_items: CraftedItems {
                crafted: HashMap::from([((item_id, 0), 99)]),
                leftovers: HashMap::from([(
                    item_id,
                    (1, leftover_cost, crafting::Source::Crafting)
                )]),
                lots: HashMap::from([(item_id, (Money::from_copper(2 * 94 + 45), 3))]),
                ..Default::default()
            },
            purchases: HashMap::from([
                (
                    (1234, TradingPost),
                    bought(66, 90, 94, 90 + 92 * 33 + 94 * 32)
                ),
                ((1235, TradingPost), bought(33, 43, 45, 43 + 45 * 32)),
            ]),
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
                vec![Recipe::mock(
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
                )],
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
        unlocked_by: HashMap::new(),
    };

    let profitable_item = calculate_crafting_profit(
        &[main_item.id],
        &game_data,
        &api_listings(tp_listings),
        None,
        &Default::default(),
    )
    .unwrap();

    assert_eq!(
        profitable_item
            .purchases
            .keys()
            .map(|&(item_id, _)| item_id)
            .collect::<HashSet<u32>>(),
        expected_purchased_ingredients
    );
    assert_eq!(
        profitable_item
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
        vec![Recipe::mock(
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
        )],
    );
    recipes_map.insert(
        2100,
        vec![Recipe::mock(
            301,
            2100,
            5,
            [],
            &[RecipeIngredient {
                item_id: 2110,
                count: 1,
            }],
            true,
        )],
    );

    let api_listings = api_listings(vec![
        (1000, vec![(100, 100), (155, 50), (200, 1)], vec![]),
        (2000, vec![], vec![(50, 100), (30, 10), (25, 1)]), // needs 2
        (2100, vec![(100, 30)], vec![(31, 100), (21, 11), (15, 1)]),
        (2110, vec![], vec![(150, 20), (125, 6), (100, 1)]), // 30, 25, 20 per crafted 2100
    ]);
    let game_data = GameData {
        recipes: recipes_map,
        items: items_map,
        unlocked_by: HashMap::new(),
    };

    let profitable_item = calculate_crafting_profit(
        &[1000],
        &game_data,
        &api_listings,
        None,
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
    let leftover_cost = Money::from_copper(30);
    let mut leftovers: HashMap<u32, (u32, Money, crafting::Source)> = HashMap::new();
    leftovers.insert(2100, (1, leftover_cost, crafting::Source::Crafting));
    let revenue = calc_revenue(vec![(50, 155), (1, 200)]);
    use crafting::Source::TradingPost;
    let mut crafted = HashMap::new();
    crafted.insert((1000, 0), 51);
    crafted.insert((2100, 0), 40);
    assert_eq!(
        profitable_item,
        Ok(ProfitableItem {
            sales: vec![Sale {
                id: 1000,
                count: 51,
                max_sell: Money::from_copper(200),
                min_sell: Money::from_copper(155),
                // (50 * 2 + 30) / (85/100)
                breakeven: Money::from_copper(153),
                profit: revenue - (crafting_cost - leftover_cost),
                stop: String::new(),
            }],
            crafting_cost,
            // the leftover sells for its cost
            profit: revenue + leftover_cost - crafting_cost,
            // the sales' transactions are 7750 and 200: 5% listing fees, halves rounded up, and
            // 10% exchange fees from the payout
            money_required: crafting_cost + Money::from_copper(388 + 10),
            payout: Some(Money::from_copper(7750 - 775 + 200 - 20)),
            // into the buy order at 100, 85 after fees; the 15 and 21 listings were bought, so the
            // 31 left lists for 26 after fees; 30 marked up for fees breaks even
            leftover_sales: HashMap::from([(
                2100,
                (
                    1,
                    Some(Money::from_copper(85)),
                    Some(Money::from_copper(26)),
                    Money::from_copper(36),
                ),
            )]),
            crafting_steps: 59,
            crafting_seconds: 2,
            // 3 ingredients, output, 2 price levels
            market_seconds: 15 + 18 + 3 + 10,
            crafted_items: CraftedItems {
                crafted,
                leftovers,
                // the last batch, from the 150 listing
                lots: HashMap::from([(2100, (Money::from_copper(150), 5))]),
                ..Default::default()
            },
            // by the steps of the cost above
            purchases: HashMap::from([
                (
                    (2000, TradingPost),
                    bought(
                        102,
                        25,
                        50,
                        25 + 30 + 30 * 8 + 30 + 50 + 50 * 2 * (11 + 30 + 4)
                    ),
                ),
                ((2100, TradingPost), bought(12, 15, 21, 15 + 21 * 11)),
                (
                    (2110, TradingPost),
                    bought(8, 100, 150, 100 + 125 * 6 + 150)
                ),
            ]),
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
    let api_listings = api_listings(vec![(1001, vec![], ingredient_sells), (1000, buys, vec![])]);
    calculate_crafting_profit(&[1000], &game_data, &api_listings, None, opt).ok()
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
    assert_eq!(profitable_item.count(), 261);
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
    assert_eq!(profitable_item.count(), 1);
    assert_eq!(profitable_item.market_seconds, 5 + 6 + 3 + 5);

    // At 100g / hour, even the first sale isn't worth the time
    assert!(at_rate(100).is_none());
}

#[test]
fn calculate_crafting_profit_min_per_hour_values_the_time_at_the_rate() {
    let at_rate = |gold| {
        one_ingredient_profit(
            vec![(1, 100)],
            vec![(1000, 1)],
            &CraftingOptions {
                min_per_hour: Some(gold),
                ..Default::default()
            },
        )
    };
    // the one sale earns 849c in 1s of crafting and 19s at the TP, so pays up to 15.28g / hour
    let item = at_rate(15).unwrap();
    assert_eq!(item.profit, Money::from_copper(849));
    assert_eq!(item.crafting_seconds + item.market_seconds, 20);
    assert!(at_rate(16).is_none());
}

#[test]
fn calculate_crafting_profit_gives_each_unit_to_the_item_it_earns_most() {
    // Items 1 and 2 are each made from one 3, and only two 3s are cheap. Planned alone, 1 takes
    // both, selling the second for 120 and leaving 2 none; together, 2's 300 beats 1's 120
    let game_data = mock_game_data(&[], &[(1, 1, &[(3, 1)]), (2, 1, &[(3, 1)])]);
    let api_listings = api_listings(vec![
        (1, vec![(120, 1000), (500, 1)], vec![]),
        (2, vec![(290, 1), (300, 1)], vec![]),
        (3, vec![], vec![(1000, 1000), (10, 2)]),
    ]);
    let plan = |item_ids: &[u32]| {
        let opt = Default::default();
        calculate_crafting_profit(item_ids, &game_data, &api_listings, None, &opt).unwrap()
    };
    assert_eq!(plan(&[1]).count(), 2);

    let profitable_item = plan(&[1, 2]);
    let made: Vec<_> = profitable_item
        .sales
        .iter()
        .map(|sale| (sale.id, sale.count))
        .collect();
    assert_eq!(made, vec![(1, 1), (2, 1)]);
    let revenue = calc_revenue(vec![(1, 500), (1, 300)]);
    assert_eq!(profitable_item.profit, revenue - Money::from_copper(20));
}

#[test]
fn calculate_crafting_profit_uses_one_items_leftovers_for_another() {
    // 1 and 2 each take a 5, and a craft of 5 makes 2, so one craft serves both
    let recipes: &[MockRecipeSpec] = &[(1, 1, &[(5, 1)]), (2, 1, &[(5, 1)]), (5, 2, &[(6, 1)])];
    let game_data = mock_game_data(&[], recipes);
    let api_listings = api_listings(vec![
        (1, vec![(500, 1)], vec![]),
        (2, vec![(500, 1)], vec![]),
        (6, vec![], vec![(100, 1000)]),
    ]);
    let opt = Default::default();
    let profitable_item =
        calculate_crafting_profit(&[1, 2], &game_data, &api_listings, None, &opt).unwrap();

    let crafted = HashMap::from([((1, 0), 1), ((2, 0), 1), ((5, 0), 2)]);
    assert_eq!(profitable_item.crafted_items.crafted, crafted);
    assert!(profitable_item.crafted_items.leftovers.is_empty());
    let revenue = calc_revenue(vec![(1, 500), (1, 500)]);
    assert_eq!(profitable_item.profit, revenue - Money::from_copper(100));
}

#[test]
fn calculate_crafting_profit_sells_an_item_asked_for_that_another_uses() {
    // 1 is made from a 5, and 5 is asked for too: each sells one, and both 5s are crafted
    let game_data = mock_game_data(&[], &[(1, 1, &[(5, 1)]), (5, 1, &[(6, 1)])]);
    let api_listings = api_listings(vec![
        (1, vec![(500, 1)], vec![]),
        (5, vec![(300, 1)], vec![]),
        (6, vec![], vec![(100, 1000)]),
    ]);
    let opt = Default::default();
    let profitable_item =
        calculate_crafting_profit(&[1, 5], &game_data, &api_listings, None, &opt).unwrap();

    let made: Vec<_> = profitable_item
        .sales
        .iter()
        .map(|sale| (sale.id, sale.count))
        .collect();
    assert_eq!(made, vec![(1, 1), (5, 1)]);
    let crafted = HashMap::from([((1, 0), 1), ((5, 0), 2)]);
    assert_eq!(profitable_item.crafted_items.crafted, crafted);
    let revenue = calc_revenue(vec![(1, 500), (1, 300)]);
    assert_eq!(profitable_item.profit, revenue - Money::from_copper(200));
}

#[test]
fn calculate_crafting_profit_makes_an_item_once_another_buys_its_cheap_listing() {
    // 1 is made from a 2. At first 2 is cheaper to buy than to craft, but once 1 buys the
    // listing at 50, crafting 2 for 100 pays against its buy orders at 300
    let game_data = mock_game_data(&[], &[(1, 1, &[(2, 1)]), (2, 1, &[(6, 1)])]);
    let api_listings = api_listings(vec![
        (1, vec![(1000, 1)], vec![]),
        (2, vec![(300, 5)], vec![(50, 1)]),
        (6, vec![], vec![(100, 1000)]),
    ]);
    let opt = Default::default();
    let profitable_item =
        calculate_crafting_profit(&[1, 2], &game_data, &api_listings, None, &opt).unwrap();

    let made: Vec<_> = profitable_item
        .sales
        .iter()
        .map(|sale| (sale.id, sale.count))
        .collect();
    assert_eq!(made, vec![(1, 1), (2, 5)]);
}

#[test]
fn calculate_crafting_profit_reports_why_an_item_asked_for_was_not_made() {
    let game_data = mock_game_data(&[], &[(1, 1, &[(3, 1)]), (2, 1, &[(3, 1)])]);
    let api_listings = api_listings(vec![
        (1, vec![(500, 1)], vec![]),
        (2, vec![], vec![]),
        (3, vec![], vec![(10, 1000)]),
    ]);
    let opt = Default::default();
    let profitable_item =
        calculate_crafting_profit(&[1, 2], &game_data, &api_listings, None, &opt).unwrap();
    let stops: Vec<_> = profitable_item
        .sales
        .iter()
        .map(|sale| (sale.id, sale.count, sale.stop.as_str()))
        .collect();
    let no_buy_orders = "No buy orders on the trading post; pass --value to price it";
    assert_eq!(stops, vec![(1, 1, ""), (2, 0, no_buy_orders)]);
}

#[test]
fn calculate_crafting_profit_min_per_hour_keeps_an_item_whose_units_earn_less_but_sell_faster() {
    // 1's first sale is worth the time, but each of its next 20 is a sale of its own, earning
    // about 6c for 5s. 2 earns about 4c each, but its 250 sell at once
    let game_data = mock_game_data(&[], &[(1, 1, &[(3, 1)]), (2, 1, &[(4, 1)])]);
    let mut buys: Vec<(u32, u32)> = (0..20).map(|i| (145 - i, 1)).rev().collect();
    buys.push((10000, 1));
    let api_listings = api_listings(vec![
        (1, buys, vec![]),
        (2, vec![(115, 250)], vec![]),
        (3, vec![], vec![(100, 1000)]),
        (4, vec![], vec![(93, 1000)]),
    ]);
    let opt = CraftingOptions {
        min_per_hour: Some(5),
        ..Default::default()
    };
    let profitable_item =
        calculate_crafting_profit(&[1, 2], &game_data, &api_listings, None, &opt).unwrap();

    let made: Vec<_> = profitable_item
        .sales
        .iter()
        .map(|sale| (sale.id, sale.count))
        .collect();
    assert_eq!(made, vec![(1, 1), (2, 250)]);
}

/// Output id, output count, and (ingredient id, count)s
type MockRecipeSpec<'a> = (u32, u32, &'a [(u32, u32)]);

/// Mock items and recipes; real item ids where `vendor_cost` matches on id
fn mock_game_data(items: &[(u32, &str, u32)], recipes: &[MockRecipeSpec]) -> GameData {
    let items = items
        .iter()
        .map(|&(id, name, value)| Item::mock(id, name, value));
    let recipe = |&(id, count, ingredients): &MockRecipeSpec| {
        let ingredients: Vec<RecipeIngredient> = ingredients
            .iter()
            .map(|&(item_id, count)| RecipeIngredient { item_id, count })
            .collect();
        Recipe::mock(id, id, count, [], &ingredients, true)
    };
    GameData::new(recipes.iter().map(recipe), items.collect())
}

/// `count` of the item, valued high
fn valued_high(
    item_id: u32,
    count: u32,
    game_data: &GameData,
    api_listings: &HashMap<u32, ItemListings>,
) -> ProfitableItem {
    let opt = CraftingOptions {
        value: Some(Some(100000)),
        count: Some(count),
        ..Default::default()
    };
    let profitable_item =
        calculate_crafting_profit(&[item_id], game_data, api_listings, None, &opt).unwrap();
    assert_eq!(profitable_item.count(), count);
    profitable_item
}

/// How many of `ingredient_id` are crafted for `count` of the item, valued high
fn crafted_ingredients(
    item_id: u32,
    count: u32,
    ingredient_id: u32,
    game_data: &GameData,
    api_listings: &HashMap<u32, ItemListings>,
) -> u32 {
    let crafted = valued_high(item_id, count, game_data, api_listings)
        .crafted_items
        .crafted;
    let crafted = crafted.iter().filter(|((id, _), _)| *id == ingredient_id);
    crafted.map(|(_, count)| count).sum()
}

/// Item 1 from one of 2, or from one of 3
fn two_recipes() -> GameData {
    mock_game_data(&[], &[(1, 1, &[(2, 1)]), (1, 1, &[(3, 1)])])
}

#[test]
fn calculate_crafting_profit_takes_the_cheaper_recipe() {
    let api_listings = api_listings(vec![
        (2, vec![], vec![(100, 1000)]),
        (3, vec![], vec![(50, 1000)]),
    ]);
    let profitable_item = valued_high(1, 1, &two_recipes(), &api_listings);
    let crafted = profitable_item.crafted_items.crafted;
    assert_eq!(crafted, HashMap::from([((1, 1), 1)]));
}

#[test]
fn calculate_crafting_profit_takes_the_faster_of_recipes_that_cost_the_same() {
    let mut game_data = two_recipes();
    game_data.recipes.get_mut(&1).unwrap()[0].time_to_craft_ms = Some(5000);
    let api_listings = api_listings(vec![
        (2, vec![], vec![(50, 1000)]),
        (3, vec![], vec![(50, 1000)]),
    ]);
    let profitable_item = valued_high(1, 1, &game_data, &api_listings);
    let crafted = profitable_item.crafted_items.crafted;
    assert_eq!(crafted, HashMap::from([((1, 1), 1)]));
}

#[test]
fn calculate_crafting_profit_takes_the_recipe_with_less_to_buy_at_the_same_cost() {
    let game_data = mock_game_data(&[], &[(1, 1, &[(2, 1), (3, 1)]), (1, 1, &[(4, 2)])]);
    let api_listings = api_listings(vec![
        (2, vec![], vec![(50, 1000)]),
        (3, vec![], vec![(50, 1000)]),
        (4, vec![], vec![(50, 1000)]),
    ]);
    let profitable_item = valued_high(1, 1, &game_data, &api_listings);
    let crafted = profitable_item.crafted_items.crafted;
    assert_eq!(crafted, HashMap::from([((1, 1), 1)]));
}

#[test]
fn calculate_crafting_profit_switches_recipes_when_the_cheaper_runs_out() {
    // 2 is the cheaper ingredient for only its first 2 listed
    let api_listings = api_listings(vec![
        (2, vec![], vec![(50, 2), (200, 1000)]),
        (3, vec![], vec![(100, 1000)]),
    ]);
    let profitable_item = valued_high(1, 4, &two_recipes(), &api_listings);
    let crafted = profitable_item.crafted_items.crafted;
    assert_eq!(crafted, HashMap::from([((1, 0), 2), ((1, 1), 2)]));
}

#[test]
fn calculate_crafting_profit_sells_while_crafting_each_recipe_of_the_output() {
    let mut game_data = two_recipes();
    for recipe in game_data.recipes.get_mut(&1).unwrap() {
        recipe.time_to_craft_ms = Some(0);
    }
    let api_listings = api_listings(vec![
        (2, vec![], vec![(50, 130), (200, 1000)]),
        (3, vec![], vec![(100, 1000)]),
    ]);
    let profitable_item = valued_high(1, 261, &game_data, &api_listings);
    let crafted = &profitable_item.crafted_items.crafted;
    assert_eq!(*crafted, HashMap::from([((1, 0), 130), ((1, 1), 131)]));
    // 2 buys, 2 ingredients, output, 2 sales less 1s of crafting by each recipe
    assert_eq!(profitable_item.market_seconds, 10 + 12 + 3 + 8);
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
    let api_listings = api_listings(vec![
        (12169, vec![], vec![(bowl_price, 10000)]),
        (12229, vec![], vec![(10, 10000)]),
    ]);
    crafted_ingredients(95421, 120, 12169, &game_data, &api_listings)
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
    let api_listings = api_listings(vec![
        (72194, vec![], vec![(patch_price, 10000)]),
        (24341, vec![], vec![(10, 10000)]),
        (19735, vec![], vec![(100, 10000)]),
        (19747, vec![], vec![(200, 10000)]),
    ]);
    crafted_ingredients(19859, 20, 72194, &game_data, &api_listings)
}

#[test]
fn calculate_crafting_profit_buys_the_own_output_a_promotion_uses() {
    // 250 Copper Ore, 1 Iron Ore and 5 dust make 88 Iron Ore
    let game_data = mock_game_data(
        &[(1, "Iron Ore", 0), (2, "Copper Ore", 0), (3, "Dust", 0)],
        &[(1, 88, &[(2, 250), (1, 1), (3, 5)])],
    );
    let api_listings = api_listings(vec![
        (1, vec![], vec![(100, 1000)]),
        (2, vec![], vec![(1, 1000)]),
        (3, vec![], vec![(1, 1000)]),
    ]);
    let crafted = crafted_ingredients(1, 88, 1, &game_data, &api_listings);
    assert_eq!(crafted, 88);
}

#[test]
fn calculate_crafting_profit_buys_at_the_price_listed_before_a_losing_promotion() {
    // An ingot from 1 ore; 1 ore and 1 dust make 2 ore, at (10 + 100) / 2 each
    let game_data = mock_game_data(
        &[(1, "Ore", 0), (2, "Dust", 0), (3, "Ingot", 0)],
        &[(3, 1, &[(1, 1)]), (1, 2, &[(1, 1), (2, 1)])],
    );
    let api_listings = api_listings(vec![
        (1, vec![], vec![(10, 1), (20, 1000)]),
        (2, vec![], vec![(100, 1000)]),
    ]);
    let opt = CraftingOptions {
        value: Some(Some(100000)),
        count: Some(1),
        ..Default::default()
    };
    let profitable_item =
        calculate_crafting_profit(&[3], &game_data, &api_listings, None, &opt).unwrap();
    assert_eq!(profitable_item.crafting_cost, Money::from_copper(10));
}

#[test]
fn calculate_crafting_profit_makes_the_count_asked_for_from_part_of_a_batch() {
    let game_data = mock_game_data(
        &[(1, "Batch of 3", 0), (2, "Part", 0)],
        &[(1, 3, &[(2, 1)])],
    );
    let api_listings = api_listings(vec![(2, vec![], vec![(10, 1000)])]);
    // 4 batches for 10, with 2 left over
    let crafted = crafted_ingredients(1, 10, 1, &game_data, &api_listings);
    assert_eq!(crafted, 12);
}

#[test]
fn calculate_crafting_profit_takes_the_leftovers_there_are_and_crafts_the_rest() {
    // 1 takes 2 of 2, crafted 3 at a time from one 3 at 30, so 10 each
    let game_data = mock_game_data(
        &[(1, "Pair", 0), (2, "Batch of 3", 0), (3, "Part", 0)],
        &[(1, 1, &[(2, 2)]), (2, 3, &[(3, 1)])],
    );
    let api_listings = api_listings(vec![(3, vec![], vec![(30, 1000)])]);
    let item = valued_high(1, 2, &game_data, &api_listings);
    // the second takes the first's leftover and 1 of another batch, leaving 2
    let leftovers = HashMap::from([(2, (2, Money::from_copper(20), crafting::Source::Crafting))]);
    assert_eq!(item.crafted_items.leftovers, leftovers);
    assert_eq!(item.crafting_cost, Money::from_copper(2 * 30));
    let breakeven = Money::from_copper(10 + 10).trading_post_listing_price();
    assert_eq!(item.sales[0].breakeven, breakeven);
}

#[test]
fn calculate_crafting_profit_buys_cheap_listings_and_crafts_the_rest() {
    let game_data = mock_game_data(
        &[(1, "Parent", 0), (2, "Part", 0), (3, "Material", 0)],
        &[(1, 1, &[(2, 10)]), (2, 1, &[(3, 1)])],
    );
    // A part costs 50 to craft; only 4 are listed below that
    let api_listings = api_listings(vec![
        (2, vec![], vec![(30, 4), (100, 1000)]),
        (3, vec![], vec![(50, 1000)]),
    ]);
    let crafted = crafted_ingredients(1, 1, 2, &game_data, &api_listings);
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

#[test]
fn leftover_sale_vendors_what_the_buy_orders_pay_less_for() {
    let listings = api_listings(vec![(1, vec![(20, 10), (3, 100)], vec![])]);
    let listings = trading_post::ItemListings::from(listings[&1].clone());
    let item = Item::mock(1, "Leftover", 4);
    // the 10 at 20c pay 170c after fees, more than 40c vendored; the next 20 at 3c pay 51c, less
    // than 80c vendored
    let copper = Money::from_copper;
    let sale = leftover_sale(30, Some(&listings), Some(&item));
    assert_eq!(sale, (10, copper(170), copper(170 + 80)));
    // with no buy orders, all are vendored
    let sale = leftover_sale(30, None, Some(&item));
    assert_eq!(sale, (0, copper(0), copper(120)));
    // and none of those vendors won't buy
    let item = r#"{"id": 1, "name": "Leftover", "vendor_value": 4, "type": "Armor",
        "rarity": "Junk", "level": 0, "flags": ["NoSell"], "restrictions": []}"#;
    let item: Item = serde_json::from_str(item).unwrap();
    let sale = leftover_sale(30, Some(&listings), Some(&item));
    assert_eq!(sale, (30, copper(170 + 51), copper(170 + 51)));
}

/// An item that unlocks `recipe_id`
fn mock_unlock_item(id: u32, recipe_id: u32) -> Item {
    let details =
        format!("{{\"Consumable\": {{\"type\": \"Unlock\", \"recipe_id\": {recipe_id}}}}}");
    let item = format!(
        "{{\"id\": {id}, \"name\": \"Recipe\", \"vendor_value\": 0, \"type\": \"Consumable\",
            \"rarity\": \"Junk\", \"level\": 0, \"flags\": [], \"restrictions\": [],
            \"details\": {details}}}"
    );
    serde_json::from_str(&item).unwrap()
}

fn ingredient(item_id: u32, count: u32) -> RecipeIngredient {
    RecipeIngredient { item_id, count }
}

/// Item 1 by recipe 10, unknown, which item 50 unlocks; then `recipes`
fn unknown_recipe(ingredients: &[RecipeIngredient], recipes: Vec<Recipe>) -> GameData {
    let mut recipes = recipes;
    recipes.insert(0, Recipe::mock(10, 1, 1, [], ingredients, false));
    GameData::new(recipes, vec![mock_unlock_item(50, 10)])
}

/// One of item 1, valued high, knowing no recipes
fn one_valued_high<T>(calculate: impl FnOnce(&HashSet<u32>, &CraftingOptions) -> T) -> T {
    let opt = CraftingOptions {
        value: Some(Some(100000)),
        count: Some(1),
        ..Default::default()
    };
    calculate(&HashSet::new(), &opt)
}

#[test]
fn calculate_crafting_profit_buys_an_unknown_recipe_with_the_rest() {
    // the unlock item is made from 5 of 2, bought after the 3 the item takes
    let sheet = Recipe::mock(20, 50, 1, [Discipline::Merchant], &[ingredient(2, 5)], true);
    let game_data = unknown_recipe(&[ingredient(2, 3)], vec![sheet]);
    let api_listings = api_listings(vec![(2, vec![], vec![(100, 3), (200, 1000)])]);
    let item = one_valued_high(|known, opt| {
        calculate_crafting_profit(&[1], &game_data, &api_listings, Some(known), opt)
    })
    .unwrap();

    let cost = Money::from_copper(3 * 100 + 5 * 200);
    assert_eq!(item.crafting_cost, cost);
    assert_eq!(item.sales[0].breakeven, cost.trading_post_listing_price());
    let entry = &item.purchases[&(2, crafting::Source::TradingPost)];
    assert_eq!((entry.count, entry.total_cost), (8, cost));
    let crafted = HashMap::from([((1, 0), 1), ((50, 0), 1)]);
    assert_eq!(item.crafted_items.crafted, crafted);
    let unlocks = BTreeMap::from([(10, Some((50, Money::from_copper(1000))))]);
    assert_eq!(item.crafted_items.unlocks, unlocks);
}

#[test]
fn calculate_crafting_profit_charges_an_unlock_only_to_the_items_that_need_it() {
    // 1 needs recipe 10, whose unlock item 50 is made from 5 of 2; 3 is made from a 2 and needs
    // no unlock, so its breakeven and profit count just its 2
    let sheet = Recipe::mock(20, 50, 1, [Discipline::Merchant], &[ingredient(2, 5)], true);
    let other = Recipe::mock(30, 3, 1, [], &[ingredient(2, 1)], true);
    let game_data = unknown_recipe(&[ingredient(2, 3)], vec![sheet, other]);
    let api_listings = api_listings(vec![(2, vec![], vec![(100, 1000)])]);
    let item = one_valued_high(|known, opt| {
        calculate_crafting_profit(&[1, 3], &game_data, &api_listings, Some(known), opt)
    })
    .unwrap();

    let sales: Vec<_> = item
        .sales
        .iter()
        .map(|sale| (sale.id, sale.breakeven, sale.profit.to_copper_value()))
        .collect();
    let listed = |copper| Money::from_copper(copper).trading_post_listing_price();
    assert_eq!(
        sales,
        vec![
            (1, listed(3 * 100 + 5 * 100), 100000 - 8 * 100),
            (3, listed(100), 100000 - 100)
        ]
    );
}

#[test]
fn calculate_crafting_profit_charges_an_unlock_for_an_unlock_to_the_item_needing_both() {
    // 1 needs recipe 10, taught by 50, which is made by recipe 20, taught by 60; 3 needs neither
    let recipes = vec![
        Recipe::mock(10, 1, 1, [], &[ingredient(2, 3)], false),
        Recipe::mock(20, 50, 1, [], &[ingredient(2, 5)], false),
        Recipe::mock(30, 3, 1, [], &[ingredient(2, 1)], true),
    ];
    let items = vec![mock_unlock_item(50, 10), mock_unlock_item(60, 20)];
    let game_data = GameData::new(recipes, items);
    let api_listings = api_listings(vec![
        (2, vec![], vec![(100, 1000)]),
        (60, vec![], vec![(300, 10)]),
    ]);
    let item = one_valued_high(|known, opt| {
        calculate_crafting_profit(&[1, 3], &game_data, &api_listings, Some(known), opt)
    })
    .unwrap();

    let profits: Vec<_> = item
        .sales
        .iter()
        .map(|sale| (sale.id, sale.profit.to_copper_value()))
        .collect();
    assert_eq!(
        profits,
        vec![(1, 100000 - 300 - 500 - 300), (3, 100000 - 100)]
    );
}

#[test]
fn calculate_crafting_profit_crafts_an_unlock_item_below_its_listings() {
    // the unlock item is 600 listed, or 5 of 2, each 200 listed or made from a 5, listed at 250
    // but made from a 3 at 10; crafting 2 can only win if 5's lower bound comes from its recipe
    let sheet = Recipe::mock(20, 50, 1, [Discipline::Merchant], &[ingredient(2, 5)], true);
    let part = Recipe::mock(21, 2, 1, [], &[ingredient(5, 1)], true);
    let sub_part = Recipe::mock(22, 5, 1, [], &[ingredient(3, 1)], true);
    let game_data = unknown_recipe(&[ingredient(4, 1)], vec![sheet, part, sub_part]);
    let api_listings = api_listings(vec![
        (2, vec![], vec![(200, 1000)]),
        (3, vec![], vec![(10, 1000)]),
        (4, vec![], vec![(100, 1000)]),
        (5, vec![], vec![(250, 1000)]),
        (50, vec![], vec![(600, 1)]),
    ]);
    let item = one_valued_high(|known, opt| {
        calculate_crafting_profit(&[1], &game_data, &api_listings, Some(known), opt)
    })
    .unwrap();
    let unlocks = BTreeMap::from([(10, Some((50, Money::from_copper(50))))]);
    assert_eq!(item.crafted_items.unlocks, unlocks);
}

/// The recipes one of item 1 is crafted by, valued high
fn capped_crafted(
    game_data: &GameData,
    api_listings: &HashMap<u32, ItemListings>,
) -> HashMap<(u32, usize), u32> {
    let item = one_valued_high(|known, opt| {
        calculate_capped_profit(&[1], game_data, api_listings, Some(known), opt)
    });
    item.unwrap().crafted_items.crafted
}

#[test]
fn calculate_capped_profit_bans_an_unknown_recipe_whose_unlock_costs_more_than_it_saves() {
    // recipe 0 (id 10) saves 20, but needs unlock item 50, costing 500
    let known = Recipe::mock(11, 1, 1, [], &[ingredient(3, 1)], true);
    let game_data = unknown_recipe(&[ingredient(2, 1)], vec![known]);
    let api_listings = api_listings(vec![
        (2, vec![], vec![(80, 1000)]),
        (3, vec![], vec![(100, 1000)]),
        (50, vec![], vec![(500, 1)]),
    ]);
    let crafted = capped_crafted(&game_data, &api_listings);
    assert_eq!(crafted, HashMap::from([((1, 1), 1)]));
}

#[test]
fn calculate_capped_profit_buys_rather_than_craft_a_batch_whose_leftovers_lose_money() {
    // 10 of 2 are made from a 3 at 100, so 9 are left over, with no buy orders to sell them to
    let game_data = mock_game_data(&[], &[(1, 1, &[(2, 1)]), (2, 10, &[(3, 1)])]);
    let api_listings = api_listings(vec![
        (2, vec![], vec![(50, 1000)]),
        (3, vec![], vec![(100, 1000)]),
    ]);
    let crafted = capped_crafted(&game_data, &api_listings);
    assert_eq!(crafted, HashMap::from([((1, 0), 1)]));
}

#[test]
fn calculate_capped_profit_buys_rather_than_craft_when_the_time_costs_more_than_it_saves() {
    // crafting 2 costs 100 for a batch of 10, 1c less than buying one, but takes 1s more
    let game_data = mock_game_data(&[], &[(1, 1, &[(2, 1)]), (2, 10, &[(3, 1)])]);
    let api_listings = api_listings(vec![
        (2, vec![], vec![(101, 1000)]),
        (3, vec![], vec![(100, 1000)]),
    ]);
    let crafted = |min_per_hour| {
        let opt = CraftingOptions {
            value: Some(Some(100000)),
            count: Some(1),
            min_per_hour,
            ..Default::default()
        };
        let item = calculate_capped_profit(&[1], &game_data, &api_listings, None, &opt);
        item.unwrap().crafted_items.crafted
    };
    assert_eq!(crafted(None), HashMap::from([((1, 0), 1), ((2, 0), 10)]));
    // 1s is worth 2.78c at 1g / hour
    assert_eq!(crafted(Some(1)), HashMap::from([((1, 0), 1)]));
}

mod data {
    use super::*;

    pub struct TestData {
        pub item_id: u32,
        pub game_data: GameData,
        pub api_listings: HashMap<u32, ItemListings>,
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
                vec![Recipe::mock(
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
                )],
            );
        }

        // Item id, buy orders and sell listings, each as (unit price, quantity)
        let api_listings = api_listings(vec![
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
                unlocked_by: HashMap::new(),
            },
            api_listings,
        }
    }
}
