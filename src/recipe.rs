use crate::api;
use crate::config::CONFIG;
use crate::custom_recipes;
use crate::money::Money;

use std::cmp::Ordering;
use std::collections::HashSet;
use std::convert::TryFrom;

use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, EnumVariantNames};

/// Moving to the next recipe in the crafting interface
const RECIPE_SWITCH_MS: u32 = 1_000;
/// Finding an ingredient on the TP or at a vendor and typing the quantity
pub const INGREDIENT_SWITCH_MS: u32 = 6_000;
/// Recipes that can no longer be learned: Wooden Sword, Asuran Harpoon
pub const UNLEARNABLE_RECIPES: [u32; 2] = [5424, 6057];

#[derive(
    Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Display, EnumString, EnumVariantNames,
)]
pub enum Discipline {
    // TODO: swap these next two, next time rebuilding data files, so alphabetical
    Artificer,
    Armorsmith,
    Chef,
    Huntsman,
    Jeweler,
    Leatherworker,
    Tailor,
    Weaponsmith,
    Scribe,
    // A few more for compatibility with custom recipes
    #[strum(serialize = "Mystic Forge")]
    MysticForge,
    #[strum(serialize = "Double Click")]
    DoubleClick,
    Salvage,
    Merchant,
    Charge,
    Achievement,
    Growing,
    Homesteader,
}

impl Discipline {
    /// Crafting professions and the Mystic Forge, each needing its station; listed first
    pub fn is_station(&self) -> bool {
        *self as u8 <= Discipline::MysticForge as u8
    }

    pub fn get_abbrev(&self) -> String {
        let s = self.to_string();
        match &s[..1] {
            // take 1st and 8th characters to distinguish Merchant/Mystic Forge
            "M" => format!("{}{}", &s[..1], &s[7..8]),
            // take 1st and 3rd characters to distinguish Scribe/Salvage and
            // Armorsmith/Artificer/Achievement
            "A" | "S" => format!("{}{}", &s[..1], &s[2..3]),
            // take 1st and 4th characters to distinguish Chef/Charge
            "C" => format!("{}{}", &s[..1], &s[3..4]),
            l => l.to_string(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RecipeSource {
    Automatic,
    Discoverable,
    Purchasable,
    Achievement,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurrencyCost {
    pub currency_id: u32, // id from /v2/currencies
    pub count: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Recipe {
    pub id: Option<u32>,
    pub output_item_id: u32,
    pub output_item_count: u32,
    pub disciplines: Vec<Discipline>,
    pub min_rating: Option<u16>, // not included in custom recipes
    pub ingredients: Vec<api::RecipeIngredient>,
    pub currencies: Vec<CurrencyCost>,
    pub time_to_craft_ms: Option<u32>, // not included in custom recipes
    source: RecipeSource,
}

impl From<api::Recipe> for Recipe {
    fn from(recipe: api::Recipe) -> Self {
        let source = if recipe.is_purchased() {
            RecipeSource::Purchasable
        } else if recipe.is_automatic() {
            RecipeSource::Automatic
        } else {
            RecipeSource::Discoverable
        };
        Recipe {
            id: Some(recipe.id),
            output_item_id: recipe.output_item_id,
            output_item_count: recipe.output_item_count,
            disciplines: recipe.disciplines,
            min_rating: Some(recipe.min_rating),
            ingredients: recipe.ingredients,
            currencies: vec![],
            time_to_craft_ms: Some(recipe.time_to_craft_ms),
            source,
        }
    }
}

impl TryFrom<custom_recipes::Recipe> for Recipe {
    type Error = String;

    fn try_from(recipe: custom_recipes::Recipe) -> Result<Self, Self::Error> {
        let output_item_count = if let Some(count) = recipe.output_item_count {
            count
        } else {
            return Err(format!(
                "Ignoring '{}'. Failed to parse 'output_item_count' as integer.",
                recipe.name
            ));
        };
        // Unless the file gives a source, any disciplines _except_ Achievement can be counted as known
        // While some regular discipline precursor recipes must be learned, the
        // outputs appear to be account bound anyway, so won't be on TP.
        // There are some useful Scribe WvW BPs in the data, so ignoring all
        // normal discipline recipes would catch those too.
        let source = if let Some(source) = recipe.source {
            source
        } else if recipe.disciplines.contains(&Discipline::Achievement) {
            RecipeSource::Achievement
        } else {
            RecipeSource::Automatic
        };
        let mut ingredients = vec![];
        let mut currencies = vec![];
        for ingredient in recipe.ingredients {
            match ingredient {
                custom_recipes::Ingredient::Item { id, count } => {
                    ingredients.push(api::RecipeIngredient { item_id: id, count })
                }
                custom_recipes::Ingredient::Currency { id, count } => {
                    currencies.push(CurrencyCost {
                        currency_id: id,
                        count,
                    })
                }
            }
        }
        Ok(Recipe {
            id: recipe.id,
            output_item_id: recipe.output_item_id,
            output_item_count,
            disciplines: recipe.disciplines,
            min_rating: None,
            ingredients,
            currencies,
            time_to_craft_ms: None,
            source,
        })
    }
}

impl Recipe {
    /// Estimated milliseconds to make `steps` batches of this recipe
    pub fn crafting_ms(&self, steps: u32) -> u32 {
        use Discipline::{
            Achievement, Charge, DoubleClick, Growing, Merchant, MysticForge, Salvage,
        };

        let has = |discipline: Discipline| self.disciplines.contains(&discipline);
        // Fit of batches timed at 1s, 2s and 7s base craft times: 2.3 times the base, plus 0.16
        // times it per batch
        let crafting_station = |base_ms: u32| {
            debug_assert_eq!(
                base_ms % 50,
                0,
                "{} ms has no whole 2.3 or 0.16 of it",
                base_ms
            );
            base_ms * 23 / 10 + base_ms * 4 / 25 * steps
        };

        if let Some(ms) = self.time_to_craft_ms {
            RECIPE_SWITCH_MS + crafting_station(ms)
        } else if has(MysticForge) {
            13_000 + 2_000 * steps // 15s for the first, 2s for each after
        } else if has(DoubleClick) || has(Salvage) {
            RECIPE_SWITCH_MS + 500 * steps
        } else if has(Merchant) {
            INGREDIENT_SWITCH_MS
        } else if has(Charge) || has(Achievement) || has(Growing) {
            0
        } else {
            RECIPE_SWITCH_MS + crafting_station(1_000)
        }
    }

    /// Total currency cost of one craft. None if any currency has no copper value.
    pub fn currency_cost(&self) -> Option<Money> {
        let mut cost = Money::default();
        for currency in &self.currencies {
            cost += Money::from_api_currency(currency.currency_id, currency.count)?;
        }
        Some(cost)
    }

    // see https://wiki.guildwars2.com/wiki/Category:Time_gated_recipes
    // for a list of time gated recipes
    pub fn is_timegated(&self) -> bool {
        self.disciplines.contains(&Discipline::Growing)
            || self.output_item_id == 46740  // Spool of Silk Weaving Thread
            || self.output_item_id == 46742  // Lump of Mithrillium
            || self.output_item_id == 46744  // Glob of Elder Spirit Residue
            || self.output_item_id == 46745  // Spool of Thick Elonian Cord
            || self.output_item_id == 66913  // Clay Pot
            || self.output_item_id == 66917  // Plate of Meaty Plant Food
            || self.output_item_id == 66923  // Plate of Piquant Plan Food
            || self.output_item_id == 67015  // Heat Stone
            || self.output_item_id == 67377  // Vial of Maize Balm
            || self.output_item_id == 79726  // Dragon Hatchling Doll Eye
            || self.output_item_id == 79763  // Gossamer Stuffing
            || self.output_item_id == 79790  // Dragon Hatchling Doll Hide
            || self.output_item_id == 79795  // Dragon Hatchling Doll Adornments
            || self.output_item_id == 79817  // Dragon Hatchling Doll Frame
            || self.output_item_id == 43772 // Charged Quartz Crystal
    }

    pub fn is_automatic(&self) -> bool {
        match &self.source {
            RecipeSource::Purchasable | RecipeSource::Achievement => false,
            // These aren't included in the API; assume you know them
            RecipeSource::Automatic | RecipeSource::Discoverable => true,
            // TODO: instead, check if account has a char with the required crafting level
            // Would require a key with the characters scope. Still wouldn't detect
            // discoverable recipes, but would detect access to them
        }
    }

    /// Whether plans may use this recipe: its currencies have a copper value, and the options,
    /// blacklists and known recipes allow it
    pub fn is_usable(&self, known_recipes: &HashSet<u32>) -> bool {
        // Profit can't be calculated unless all of the recipe's currencies have a copper value
        if self.currency_cost().is_none() {
            return false;
        }
        if !CONFIG.include_mystic_forge && self.disciplines.contains(&Discipline::MysticForge) {
            return false;
        }
        let unknown_id = self.filter_unknown_id(known_recipes);
        if unknown_id.is_some_and(|id| UNLEARNABLE_RECIPES.contains(&id)) {
            return false;
        }
        if !CONFIG.include_unknown_recipes && unknown_id.is_some() {
            return false;
        }
        if let Some(output_blacklist) = &CONFIG.output_blacklist {
            if output_blacklist.contains(&self.output_item_id) {
                return false;
            }
        }
        if let Some(input_blacklist) = &CONFIG.input_blacklist {
            for ingredient in &self.ingredients {
                if input_blacklist.contains(&ingredient.item_id) {
                    return false;
                }
            }
        }

        true
    }

    /// The recipe's id, if the account doesn't know it
    pub fn filter_unknown_id(&self, known_recipes: &HashSet<u32>) -> Option<u32> {
        self.id
            .filter(|id| !self.is_automatic() && !known_recipes.contains(id))
    }

    pub fn sorted_ingredients(&self) -> Vec<&api::RecipeIngredient> {
        let mut ingredients: Vec<&api::RecipeIngredient> = self.ingredients.iter().collect();
        ingredients.sort_unstable_by(|a, b| match b.count.cmp(&a.count) {
            Ordering::Equal => b.item_id.cmp(&a.item_id),
            v => v,
        });
        ingredients
    }

    /// Whether `other` makes as many from the same, by the same disciplines
    pub fn is_same_as(&self, other: &Recipe) -> bool {
        self.output_item_count == other.output_item_count
            && self.disciplines == other.disciplines
            && self.sorted_ingredients() == other.sorted_ingredients()
            && self.currencies == other.currencies
    }
}
