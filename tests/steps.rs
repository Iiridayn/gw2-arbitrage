// only the recipe mock, as the rest of common would go unused here
#[path = "common/recipe.rs"]
mod mock_recipe;

use mock_recipe::MockRecipe;

use gw2_arbitrage::{
    api::RecipeIngredient,
    crafting::{CraftedItems, GameData},
    money::Money,
    recipe::{
        Discipline::{self, *},
        Recipe,
    },
    steps,
};

use std::collections::{BTreeMap, HashMap};

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
        ..Default::default()
    };
    let recipes_map = GameData::new(recipes, vec![]).recipes;
    let mut steps = crafted.sorted(&[item_id], &recipes_map);
    let step_groups = steps::discipline_groups(&mut steps, &crafted);
    let labels = steps::disciplines_labels(&steps, &step_groups);
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
    steps::discipline_groups(&mut steps, &Default::default());
    let ids: Vec<_> = steps.iter().map(|step| step.0).collect();
    assert_eq!(ids, [4, 5, 2, 3, 1]);
}

#[test]
fn steps_learn_a_recipe_before_using_it() {
    // item 1's recipe is learned from item 50, made at the Mystic Forge; item 1 takes more, so
    // it's sorted first
    let recipes = vec![
        recipe(1, [Artificer], 400, &[2, 4]),
        recipe(50, [MysticForge], 0, &[3]),
    ];
    let crafted = CraftedItems {
        crafted: HashMap::from([((1, 0), 1), ((50, 0), 1)]),
        unlocks: BTreeMap::from([(1, Some((50, Money::from_copper(0))))]),
        ..Default::default()
    };
    let recipes_map = GameData::new(recipes, vec![]).recipes;
    let mut steps = crafted.sorted(&[1], &recipes_map);
    steps::discipline_groups(&mut steps, &crafted);
    let ids: Vec<_> = steps.iter().map(|step| step.0).collect();
    assert_eq!(ids, [50, 1]);
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
fn steps_list_each_recipe_of_an_item() {
    let crafted = CraftedItems {
        crafted: HashMap::from([((1, 0), 2), ((2, 0), 1), ((2, 1), 1)]),
        ..Default::default()
    };
    let recipes = vec![
        recipe(1, [Artificer], 400, &[2]),
        recipe(2, [Armorsmith], 400, &[]),
        recipe(2, [Tailor], 400, &[]),
    ];
    let recipes_map = GameData::new(recipes, vec![]).recipes;
    let steps = crafted.sorted(&[1], &recipes_map);
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
