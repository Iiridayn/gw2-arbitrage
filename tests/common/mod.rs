use gw2_arbitrage::{
    api::{ItemListings, Listing, RecipeIngredient},
    crafting::GameData,
    item::Item,
    recipe::Recipe,
};

use std::collections::HashMap;

mod recipe;
pub use recipe::MockRecipe;

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

/// Items and recipes for an item made from one of an ingredient
pub fn one_ingredient_game_data(
    item_id: u32,
    ingredient_id: u32,
    ingredient_vendor_value: u32,
) -> GameData {
    let mut items_map = HashMap::new();
    items_map.insert(item_id, Item::mock(item_id, "Main item", 0));
    items_map.insert(
        ingredient_id,
        Item::mock(ingredient_id, "Ingredient", ingredient_vendor_value),
    );

    let mut recipes_map = HashMap::new();
    recipes_map.insert(
        item_id,
        vec![Recipe::mock(
            1,
            item_id,
            1,
            [],
            &[RecipeIngredient {
                item_id: ingredient_id,
                count: 1,
            }],
            true,
        )],
    );
    GameData {
        recipes: recipes_map,
        items: items_map,
        unlocked_by: HashMap::new(),
    }
}

/// Item id, buy orders and sell listings, each as (unit price, quantity)
type MockListings = (u32, Vec<(u32, u32)>, Vec<(u32, u32)>);

pub fn tp_listings_map(from: Vec<MockListings>) -> HashMap<u32, ItemListings> {
    let mut map = HashMap::new();
    for (id, mut buys, mut sells) in from.into_iter() {
        buys.sort_by(|(price1, _), (price2, _)| price1.cmp(price2));
        sells.sort_by(|(price1, _), (price2, _)| price1.cmp(price2).reverse());
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
