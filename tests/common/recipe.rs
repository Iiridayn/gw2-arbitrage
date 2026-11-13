use gw2_arbitrage::{
    api::{self, RecipeIngredient},
    recipe::{Discipline, Recipe},
};

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
