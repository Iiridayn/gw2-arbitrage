use serde::{de::Deserializer, Deserialize};
use serde_json::Value;

use crate::config;
use crate::recipe;

use std::convert::TryFrom;
use std::str::FromStr;

use phf::phf_set;

// Alternate recipes encode their output as this plus the real item id
const ALTERNATE_RECIPE_ID_OFFSET: u64 = 1_000_000_000_000;

const ADDITIONAL_RECIPES: &str = include_str!("../data/additional-recipes.json");

// Bad recipes; blacklist based on item ID
static BLACKLIST_ITEM_IDS: phf::Set<u32> = phf_set! {
    // Non-integer output, probabalistic:
    19675_u32, // Mystic Clover
    38131_u32, // Delicate Snowflake
    38132_u32, // Glittering Snowflake
    38133_u32, // Unique Snowflake
    38134_u32, // Pristine Snowflake
    38135_u32, // Flawless Snowflake

    // Integer output is wrong, probabalistic:
    38121_u32, // Endless Gift Dolyak Tonic; 1/3 chance
    28115_u32, // Endless Toymaker's Tonic; 1/3 chance
    45008_u32, // Mini Steamrider; 1/3 chance
    45009_u32, // Mini Steam Hulk; 1/3 chance
    45010_u32, // Mini Steam Minotaur; 1/3 chance

    // Unclear if the halloween ingredients are random; but will probably never
    // be worth converting anyway, so leaving them off

    // Leaving in; at least lists the minimum accurately.
    // 68063_u32, // Amalgamated Gemstone; 10% chance of 25 from the 10 recipe, etc.

    // Vendor purchases using items _and currency_; currency is ignored.
    // I think most aren't a problem as outputs typically cannot be sold
    24_u32, // Sealed Package of Snowballs; 1 Snowflake, yes, but +7k karma
    // Swim Speed Infusions: +448 copper per level per conversion (fewer conversions is better)
    87518_u32, // Swim-Speed Infusion +11
    87493_u32, // Swim-Speed Infusion +12
    87503_u32, // Swim-Speed Infusion +13
    87526_u32, // Swim-Speed Infusion +14
    87496_u32, // Swim-Speed Infusion +15
    87497_u32, // Swim-Speed Infusion +16
    87508_u32, // Swim-Speed Infusion +17
    87516_u32, // Swim-Speed Infusion +18
    87532_u32, // Swim-Speed Infusion +19
    87495_u32, // Swim-Speed Infusion +20
    87525_u32, // Swim-Speed Infusion +21
    87511_u32, // Swim-Speed Infusion +22
    87512_u32, // Swim-Speed Infusion +23
    87527_u32, // Swim-Speed Infusion +24
    87502_u32, // Swim-Speed Infusion +25

    // Purchase restrictions the API doesn't expose:
    82376_u32, // Zunja's Necklace; once per account
    // Mist Champion skins; need the champion unlocked, and can't be bought once the skin is used
    72077_u32, // Shadow Assassin Nika
    72772_u32, // Rogue Grymm Svaard
    73002_u32, // Sneakthief Nika
    73989_u32, // Priory Grymm Svaard
    74423_u32, // Flamewalker Grymm Svaard
    74686_u32, // Phalanx Turai Ossa
    76274_u32, // Strider's Nika
    76461_u32, // Rampart Turai Ossa
    76488_u32, // Radiant Turai Ossa
    77684_u32, // Steampunk Tybalt
    77691_u32, // Elite Glorious Tybalt
    77704_u32, // Orrian Armor Tybalt
};

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum Ingredient {
    Item { id: u32, count: u32 },
    Currency { id: u32, count: u32 },
}

#[derive(Debug, Deserialize)]
pub struct Recipe {
    pub name: String, // used only in error output
    #[serde(deserialize_with = "real_item_id")]
    pub output_item_id: u32,
    #[serde(deserialize_with = "treat_error_as_none")]
    pub output_item_count: Option<u32>,
    #[serde(deserialize_with = "strum_discipline")]
    pub disciplines: Vec<config::Discipline>,
    pub ingredients: Vec<Ingredient>,
}

pub fn additional_recipes() -> serde_json::Result<Vec<Recipe>> {
    serde_json::from_str(ADDITIONAL_RECIPES)
}

pub async fn fetch_custom_recipes(
    notify: Option<&dyn Fn(&str)>,
) -> Result<Vec<recipe::Recipe>, Box<dyn std::error::Error>> {
    let url = "https://raw.githubusercontent.com/Peureki/custom-recipes/refs/heads/schema-update/recipes.json";
    if let Some(notify) = notify {
        notify(&url);
    }

    let custom_recipes: Vec<Recipe> = reqwest::get(url).await?.json().await?;

    let recipes: Vec<recipe::Recipe> = custom_recipes
        .into_iter()
        .chain(additional_recipes()?)
        // Remove blacklisted recipes here to avoid printing errors for non-integers
        .filter(|r| !BLACKLIST_ITEM_IDS.contains(&r.output_item_id))
        .map(std::convert::TryFrom::try_from)
        .filter_map(|result: Result<recipe::Recipe, _>| match result {
            Ok(recipe) => Some(recipe),
            Err(e) => {
                eprintln!("{}", e);
                None
            }
        })
        .collect();

    Ok(recipes)
}

fn real_item_id<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let id = u64::deserialize(deserializer)?;
    u32::try_from(id % ALTERNATE_RECIPE_ID_OFFSET).map_err(Error::custom)
}

fn treat_error_as_none<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    let value: Value = Deserialize::deserialize(deserializer)?;
    Ok(T::deserialize(value).ok())
}

fn strum_discipline<'de, D>(deserializer: D) -> Result<Vec<config::Discipline>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let value: Value = Deserialize::deserialize(deserializer)?;
    match value {
        Value::Array(vec) => {
            let mut c: Vec<config::Discipline> = Vec::new();
            for val in vec.iter() {
                if let Value::String(s) = val {
                    c.push(
                        config::Discipline::from_str(s).map_err(|e| {
                            Error::custom(format!("Unknown string \"{}\": {}", s, e))
                        })?,
                    );
                } else {
                    return Err(Error::custom("Invalid discipline - not a string"));
                }
            }
            Ok(c)
        }
        _ => Err(Error::custom("Invalid discipline - not an array")),
    }
}
