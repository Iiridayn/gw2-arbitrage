use gw2_arbitrage::{
    api::{self, ItemListings, Listing, RecipeIngredient},
    config::Discipline,
    item::Item,
    recipe::Recipe,
};

use std::collections::HashMap;

pub trait MockItem {
    fn mock(id: u32, name: &str, vendor_value: u32) -> Self;
}
impl MockItem for Item {
    fn mock(id: u32, name: &str, vendor_value: u32) -> Self {
        serde_json::from_str(
            format!(
                "{{
                    \"id\": {},
                    \"name\": \"{}\",
                    \"vendor_value\": {},
                    \"type\": \"Armor\",
                    \"rarity\": \"Junk\",
                    \"level\": 0,
                    \"flags\": [],
                    \"restrictions\": []
                }}",
                id, name, vendor_value
            )
            .as_str(),
        )
        .unwrap()
    }
}

pub trait MockRecipe {
    fn mock<const A: usize>(
        id: u32,
        output_item_id: u32,
        output_item_count: u32,
        disciplines: [Discipline; A],
        ingredients: &[RecipeIngredient],
        automatic: bool,
    ) -> Self;
}
impl MockRecipe for Recipe {
    fn mock<const A: usize>(
        id: u32,
        output_item_id: u32,
        output_item_count: u32,
        disciplines: [Discipline; A],
        ingredients: &[RecipeIngredient],
        automatic: bool,
    ) -> Self {
        let mut recipe: api::Recipe = serde_json::from_str(
            format!(
                "{{
                    \"id\": {},
                    \"output_item_id\": {},
                    \"output_item_count\": {},
                    \"time_to_craft_ms\": 0,
                    \"disciplines\": [],
                    \"min_rating\": 0,
                    \"flags\": {},
                    \"ingredients\": []
                }}",
                id,
                output_item_id,
                output_item_count,
                if automatic {
                    "[\"AutoLearned\"]"
                } else {
                    "[\"LearnedFromItem\"]"
                }
            )
            .as_str(),
        )
        .unwrap();
        recipe.disciplines = disciplines.to_vec();
        recipe.ingredients = ingredients.to_vec();
        recipe.into()
    }
}

/// Items and recipes for an item made from one of an ingredient
pub fn one_ingredient_maps(
    item_id: u32,
    ingredient_id: u32,
    ingredient_vendor_value: u32,
) -> (HashMap<u32, Item>, HashMap<u32, Recipe>) {
    let mut items_map = HashMap::new();
    items_map.insert(item_id, Item::mock(item_id, "Main item", 0));
    items_map.insert(
        ingredient_id,
        Item::mock(ingredient_id, "Ingredient", ingredient_vendor_value),
    );

    let mut recipes_map = HashMap::new();
    recipes_map.insert(
        item_id,
        Recipe::mock(
            1,
            item_id,
            1,
            [],
            &[RecipeIngredient {
                item_id: ingredient_id,
                count: 1,
            }],
            true,
        ),
    );
    (items_map, recipes_map)
}

pub fn tp_listings_map(
    from: Vec<(u32, Vec<(u32, u32)>, Vec<(u32, u32)>)>,
) -> HashMap<u32, ItemListings> {
    let mut map = HashMap::new();
    for (id, mut buys, mut sells) in from.into_iter() {
        buys.sort_by(|(price1, _), (price2, _)| price1.cmp(&price2));
        sells.sort_by(|(price1, _), (price2, _)| price1.cmp(&price2).reverse());
        map.insert(
            id,
            ItemListings {
                id,
                buys: buys
                    .into_iter()
                    .map(|(unit_price, quantity)| Listing {
                        listings: quantity,
                        unit_price,
                        quantity,
                    })
                    .collect(),
                sells: sells
                    .into_iter()
                    .map(|(unit_price, quantity)| Listing {
                        listings: quantity,
                        unit_price,
                        quantity,
                    })
                    .collect(),
            },
        );
    }
    map
}
