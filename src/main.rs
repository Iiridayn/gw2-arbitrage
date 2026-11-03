use colored::Colorize;
use serde::Serialize;

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io;
use std::io::prelude::*;

use config::CONFIG;
use gw2_arbitrage::*;
use item::{Item, ITEM_STACK_SIZE};
use money::Money;
use recipe::Recipe;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let notify_print = |url: &str| println!("Fetching {}", url);
    let notify = Some(&notify_print as &dyn Fn(&str));

    let known_recipes = if let Some(key) = &CONFIG.api_key {
        match request::fetch_account_recipes(&key, &CONFIG.cache_dir, notify).await {
            Ok(recipes) => Some(recipes),
            Err(error) => {
                eprintln!("API error fetching recipe unlocks: {}", error);
                None
            }
        }
    } else {
        None
    };

    println!("Loading recipes");
    let api_recipes = {
        let mut api_recipes: Vec<api::Recipe> = request::get_data(
            &CONFIG.api_recipes_file,
            request::API_RECIPES_MIN_TIMESTAMP,
            || request::request_paginated("recipes", &None, notify),
        )
        .await?;
        // If a recipe has no disciplines it cannot be crafted or discovered.
        // This appears to be used to mark deprecated recipes in the API.
        api_recipes.retain(|recipe| recipe.disciplines.len() > 0);
        api_recipes
    };
    println!(
        "Loaded {} recipes stored at '{}'",
        api_recipes.len(),
        CONFIG.api_recipes_file.display()
    );

    println!("Loading custom recipes");
    let custom_recipes: Vec<Recipe> = request::get_data(
        &CONFIG.custom_recipes_file,
        request::CUSTOM_RECIPES_MIN_TIMESTAMP,
        || custom_recipes::fetch_custom_recipes(notify),
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("Failed to fetch custom recipes: {}", e);
        vec![]
    });
    println!(
        "Loaded {} custom recipes stored at '{}'",
        custom_recipes.len(),
        CONFIG.custom_recipes_file.display()
    );

    println!("Loading items");
    let items: Vec<Item> = request::get_data(
        &CONFIG.items_file,
        request::ITEMS_MIN_TIMESTAMP,
        || async {
            let api_items: Vec<api::ApiItem> =
                request::request_paginated("items", &CONFIG.lang, notify).await?;
            Ok(api_items
                .into_iter()
                .map(|api_item| Item::from(api_item))
                .collect())
        },
    )
    .await?;
    println!(
        "Loaded {} items stored at '{}'",
        items.len(),
        CONFIG.items_file.display()
    );

    let recipes: Vec<Recipe> = custom_recipes
        .into_iter()
        // prefer api recipes over custom recipes if they share the same output item id, by inserting them later
        .chain(api_recipes.into_iter().map(std::convert::From::from))
        .filter(|recipe| {
            // Profit can't be calculated unless all of the recipe's currencies have a copper value
            if recipe.currency_cost().is_none() {
                return false;
            }
            if let Some(recipe_blacklist) = &CONFIG.recipe_blacklist {
                if let Some(id) = recipe.id {
                    if recipe_blacklist.contains(&id) {
                        return false;
                    }
                }
            }
            if let Some(item_blacklist) = &CONFIG.item_blacklist {
                for ingredient in &recipe.ingredients {
                    if item_blacklist.contains(&ingredient.item_id) {
                        return false;
                    }
                }
            }

            true
        })
        .collect();
    let mut recipes_map = profit::vec_to_map(recipes, |x| x.output_item_id);
    let items_map = profit::vec_to_map(items, |x| x.id);

    let recursive_recipes = recipe::mark_recursive_recipes(&recipes_map);
    for recipe_id in recursive_recipes.into_iter() {
        recipes_map.remove(&recipe_id);
    }

    let unlocks = crafting::RecipeUnlocks::new(&items_map, &known_recipes);

    if let Some(item_id) = CONFIG.item_id {
        let (profitable_item, purchased_ingredients, tp_listings_map) =
            profit::calc_item_profit(item_id, &recipes_map, &items_map, &unlocks, notify).await?;
        print_profitable_item(
            item_id,
            &profitable_item,
            &purchased_ingredients,
            &tp_listings_map,
            &recipes_map,
            &items_map,
            &known_recipes,
        )?;
    } else {
        println!("Loading trading post prices");
        print!("Pages:");
        let commerce_notify = |url: &str| {
            print!(" {}", &url[51..url.len() - 14]);
            io::stdout()
                .flush()
                .unwrap_or_else(|e| println!("Flush failed: {}", &e));
        };
        let tp_prices: Vec<api::Price> = request::request_paginated(
            "commerce/prices",
            &None,
            Some(&commerce_notify as &dyn Fn(&str)),
        )
        .await?;
        println!("");
        println!("Loaded {} trading post prices", tp_prices.len());

        let tp_prices_map = profit::vec_to_map(tp_prices, |x| x.id);

        let (profitable_item_ids, ingredient_ids) =
            profit::find_profitable_items(&tp_prices_map, &recipes_map, &items_map);

        println!("Loading detailed trading post listings");
        let mut request_listing_item_ids = vec![];
        request_listing_item_ids.extend(&profitable_item_ids);
        request_listing_item_ids.extend(ingredient_ids);
        request_listing_item_ids.sort_unstable();
        request_listing_item_ids.dedup();
        // Caching these is pointless, as the vector changes on each run, leading to new URLs
        let tp_listings =
            request::fetch_item_listings(&request_listing_item_ids, None, notify).await?;
        println!(
            "Loaded {} detailed trading post listings",
            tp_listings.len()
        );
        let tp_listings_map = profit::vec_to_map(tp_listings, |x| x.id);

        let profitable_items = profit::profitable_item_list(
            &tp_listings_map,
            &profitable_item_ids,
            &request_listing_item_ids,
            &recipes_map,
            &items_map,
            &tp_prices_map,
            &unlocks,
        );

        print_item_list(&profitable_items, &recipes_map, &items_map)?;
    }

    Ok(())
}

/// Print detailed information about a profitable item
fn print_profitable_item(
    item_id: u32,
    profitable_item: &Option<(profit::ProfitableItem, crafting::RequiredUnlocks)>,
    purchased_ingredients: &HashMap<(u32, crafting::Source), crafting::PurchasedIngredient>,
    tp_listings_map: &HashMap<u32, api::ItemListings>,
    recipes_map: &HashMap<u32, Recipe>,
    items_map: &HashMap<u32, Item>,
    known_recipes: &Option<HashSet<u32>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (profitable_item, required_unlocks) = if let Some(item) = profitable_item {
        item
    } else {
        println!("Item is not profitable to craft");
        return Ok(());
    };

    println!("============");
    println!(
        "Shopping list for {} x {} = {} profit ({} / step, {}%)",
        profitable_item.count,
        items_map
            .get(&item_id)
            .map_or_else(|| "???".to_string(), |item| item.to_string()),
        Money::from_copper(profitable_item.profit.to_copper_value()),
        profitable_item.profit_per_crafting_step().to_copper_value(),
        (profitable_item.profit_on_cost() * 100_f64).round(),
    );
    let price_msg = if profitable_item.max_sell == profitable_item.min_sell {
        format!("{}", profitable_item.min_sell)
    } else {
        format!(
            "{} to {}",
            profitable_item.max_sell, profitable_item.min_sell,
        )
    };
    // --value replaces the buy orders, so they say nothing about this sale
    let sale_sparkline = match tp_listings_map.get(&item_id) {
        Some(listings) if CONFIG.crafting.value.is_none() => format!(
            " {}",
            profit::listing_sparkline(&listings.buys, profitable_item.count)
        ),
        _ => String::new(),
    };
    println!(
        "Sell at: {}{}, Money Required: {}, Breakeven price: {}",
        price_msg,
        sale_sparkline,
        profitable_item.crafting_cost.increase_by_listing_fee(),
        profitable_item.breakeven,
    );

    println!("============");
    let mut sorted_ingredients: Vec<(&(u32, crafting::Source), &crafting::PurchasedIngredient)> =
        purchased_ingredients.iter().collect();
    sorted_ingredients.sort_unstable_by(|a, b| {
        if b.0 .1 == a.0 .1 {
            match b.1.count.cmp(&a.1.count) {
                Ordering::Equal => match b.1.total_cost.cmp(&a.1.total_cost) {
                    Ordering::Equal => b.0 .0.cmp(&a.0 .0),
                    v => v,
                },
                v => v,
            }
        } else if b.0 .1 == crafting::Source::Vendor {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    });
    let mut inventory = 0;
    for ((ingredient_id, ingredient_source), ingredient) in sorted_ingredients {
        let purchase_count = if *ingredient_source == crafting::Source::Vendor {
            items_map
                .get(ingredient_id)
                .unwrap_or_else(|| panic!("Missing item for ingredient {}", ingredient_id))
                .vendor_cost()
                .unwrap_or((Money::from_copper(0), 1))
                .1
        } else {
            ITEM_STACK_SIZE
        };
        let ingredient_count_msg = if purchase_count > 1 && ingredient.count > purchase_count {
            let stack_count = ingredient.count / purchase_count;
            inventory += ingredient.count.div_ceil(ITEM_STACK_SIZE);
            let remainder = ingredient.count % purchase_count;
            let remainder_msg = if remainder != 0 {
                format!(" + {}", remainder)
            } else {
                "".to_string()
            };
            format!(
                "{} ({} x {}{})",
                ingredient.count, stack_count, purchase_count, remainder_msg
            )
        } else {
            inventory += 1;
            ingredient.count.to_string()
        };
        let source_msg = match *ingredient_source {
            crafting::Source::TradingPost => {
                let sparkline = tp_listings_map
                    .get(ingredient_id)
                    .map_or_else(String::new, |listings| {
                        profit::listing_sparkline(&listings.sells, ingredient.count)
                    });
                if ingredient.max_price == ingredient.min_price {
                    format!(
                        " (at {}) Subtotal: {} {}",
                        ingredient.min_price, ingredient.total_cost, sparkline,
                    )
                } else {
                    format!(
                        " (at {} to {}) Subtotal: {} {}",
                        ingredient.min_price,
                        ingredient.max_price,
                        ingredient.total_cost,
                        sparkline,
                    )
                }
            }
            crafting::Source::Vendor => {
                let vendor_cost = items_map
                    .get(ingredient_id)
                    .unwrap_or_else(|| panic!("Missing item for ingredient {}", ingredient_id))
                    .vendor_cost();
                if let Some((cost, purchase_count)) = vendor_cost {
                    if purchase_count > 1 {
                        format!(
                            " (vendor: {} per {}) Subtotal: {}",
                            cost * purchase_count,
                            purchase_count,
                            cost * ingredient.count,
                        )
                    } else {
                        format!(" (vendor: {}) Subtotal: {}", cost, cost * ingredient.count,)
                    }
                } else {
                    "".to_string()
                }
            }
            crafting::Source::Crafting => "".to_string(),
        };
        println!(
            "{} {}{}",
            ingredient_count_msg,
            items_map
                .get(ingredient_id)
                .map_or_else(|| "???".to_string(), |item| item.to_string()),
            source_msg,
        );
    }

    println!("============");
    println!("Max inventory slots: {}", inventory + 1); // + 1 for the crafting output
    let crafting_seconds = profitable_item.crafted_items.crafting_seconds(&recipes_map);
    let market_seconds = profitable_item.market_seconds();
    println!(
        "Time: {} ({} crafting, {} market, {} / hour)",
        format_duration(crafting_seconds + market_seconds),
        format_duration(crafting_seconds),
        format_duration(market_seconds),
        profitable_item.profit_per_hour(&recipes_map),
    );
    println!(
        "Crafting steps: https://gw2efficiency.com/crafting/calculator/a~1!b~1!c~1!d~{}-{}",
        profitable_item.count, item_id
    );
    for (item_id, count, recipe) in profitable_item.crafted_items.sorted(item_id, &recipes_map) {
        let num_crafted = count / recipe.output_item_count;
        let item_name = items_map
            .get(&item_id)
            .map_or_else(|| "???".to_string(), |item| item.to_string());
        let mut ingredients = recipe
            .sorted_ingredients()
            .iter()
            .map(|ingredient| {
                let ingredient_name = items_map
                    .get(&ingredient.item_id)
                    .map_or_else(|| "???".to_string(), |item| item.to_string());
                format!("{} {}", ingredient.count * num_crafted, ingredient_name)
            })
            .collect::<Vec<String>>();
        if !recipe.currencies.is_empty() {
            if let Some(cost) = recipe.currency_cost() {
                ingredients.push((cost * num_crafted).to_string());
            }
        }
        let ingredients = ingredients.join(" ");
        if recipe.output_item_count > 1 {
            println!(
                "{} (makes {}) {} from {}",
                num_crafted, count, item_name, ingredients
            );
        } else {
            println!("{} {} from {}", count, item_name, ingredients);
        }
    }

    if !required_unlocks.is_empty() {
        let req_recipes = required_unlocks
            .iter()
            .map(|(recipe_id, unlock)| match unlock {
                Some((unlock_id, cost)) => format!(
                    "{}, {}",
                    items_map
                        .get(unlock_id)
                        .map_or_else(|| "???".to_string(), |item| item.to_string()),
                    cost
                ),
                // recipe 5424 for item 29407 has no unlock item, possibly others
                None => format!("Recipe {} has no unlock item with a price", recipe_id),
            })
            .collect::<Vec<String>>()
            .join("\n");
        println!(
            "You {} craft this yet. Required recipes{}:\n{}",
            match known_recipes {
                Some(_) => "can not",
                None => "may not be able to",
            },
            if required_unlocks.len() > 1 { "s" } else { "" },
            req_recipes,
        );
    }

    if !profitable_item.crafted_items.leftovers.is_empty() {
        println!("Leftovers:");
        for (leftover_id, (count, cost, _)) in profitable_item.crafted_items.leftovers.iter() {
            println!(
                "{} {}, breakeven: {} each",
                count,
                items_map
                    .get(&leftover_id)
                    .map_or_else(|| "???".to_string(), |item| item.to_string()),
                cost.trading_post_listing_price(),
            );
        }
    }

    return Ok(());
}

#[derive(Debug, Serialize)]
struct OutputRow {
    name: String,
    disciplines: String,
    item_id: u32,
    unknown_recipes: Vec<u32>,
    total_profit: String,
    number_required: u32,
    profit_per_item: i32,
    crafting_steps: u32,
    profit_per_step: i32,
    crafting_seconds: f64,
    market_seconds: f64,
    profit_per_hour: i32,
    profit_on_cost: f64,
}

fn format_duration(seconds: f64) -> String {
    let seconds = seconds.round() as u32;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

/// List profitable items to screen or CSV
fn print_item_list(
    profitable_items: &Vec<(profit::ProfitableItem, crafting::RequiredUnlocks)>,
    recipes_map: &HashMap<u32, Recipe>,
    items_map: &HashMap<u32, Item>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut csv_writer = if let Some(path) = &CONFIG.output_csv {
        Some(csv::Writer::from_path(path)?)
    } else {
        None
    };

    let mut line_colors = [
        colored::Color::Red,
        colored::Color::Green,
        colored::Color::Yellow,
        colored::Color::Magenta,
        colored::Color::Cyan,
    ]
    .iter()
    .cycle();

    let header = format!(
        "{:<50} {:<15} {:<15} {:<20} {:>15} {:>15} {:>15} {:>15} {:>15} {:>15}",
        "Name",
        "Disciplines",
        "Item id",
        "Req. Recipe Ids",
        "Total profit",
        "No. required",
        "Profit / item",
        "Time",
        "Profit / hour",
        "Profit on cost",
    );

    println!("{}", header);
    println!("{}", "=".repeat(header.len()));
    for (profitable_item, required_unlocks) in profitable_items {
        // Only required when prices are cached.
        // Profit may end up being 0, since potential profitable items are selected based
        // on cached prices, but the actual profit is calculated using detailed listings and
        // prices may have changed since they were cached.
        if profitable_item.count == 0 {
            continue;
        }

        let item_id = profitable_item.id;
        let name = items_map
            .get(&item_id)
            .map_or_else(|| "???".to_string(), |item| item.to_string());

        let recipe = recipes_map.get(&item_id).expect("Missing recipe");

        let crafting_seconds = profitable_item.crafted_items.crafting_seconds(&recipes_map);
        let profit_per_hour = profitable_item.profit_per_hour(&recipes_map);

        let output_row = OutputRow {
            name: name.to_string(),
            disciplines: recipe
                .disciplines
                .iter()
                .map(|d| d.get_abbrev())
                .collect::<Vec<_>>()
                .join("/"),
            item_id,
            unknown_recipes: required_unlocks.keys().copied().collect(),
            total_profit: profitable_item.profit.to_string(),
            number_required: profitable_item.count,
            profit_per_item: profitable_item.profit_per_item().to_copper_value(),
            crafting_steps: profitable_item.crafting_steps,
            profit_per_step: profitable_item.profit_per_crafting_step().to_copper_value(),
            crafting_seconds,
            market_seconds: profitable_item.market_seconds(),
            profit_per_hour: profit_per_hour.to_copper_value(),
            profit_on_cost: profitable_item.profit_on_cost(),
        };

        if let Some(writer) = &mut csv_writer {
            writer.serialize(&output_row)?;
        }

        let line = format!(
            "{:<50} {:<15} {:<15} {:<20} {:>15} {:>15} {:>15} {:>15} {:>15} {:>15}",
            output_row.name,
            output_row.disciplines,
            format!("{}", output_row.item_id),
            required_unlocks
                .iter()
                .map(|(recipe_id, unlock)| match unlock {
                    Some((_, cost)) => format!("{} ({})", recipe_id, cost),
                    None => format!("{} (?)", recipe_id),
                })
                .collect::<Vec<String>>()
                .join(","),
            output_row.total_profit,
            format!(
                "{} item{}",
                output_row.number_required,
                if output_row.number_required > 1 {
                    "s"
                } else {
                    ""
                }
            ),
            format!("{} / item", output_row.profit_per_item),
            format_duration(output_row.crafting_seconds + output_row.market_seconds),
            format!("{}/hr", profit_per_hour),
            format!("{}%", (output_row.profit_on_cost * 100_f64).round())
        );

        println!("{}", line.color(*line_colors.next().unwrap()));
    }

    println!("{}", "=".repeat(header.len()));
    println!("{}", header);
    println!("{}", "=".repeat(header.len()));

    let total_profit: Money = profitable_items.iter().map(|(item, _)| item.profit).sum();
    println!("Total: {}", total_profit);
    if CONFIG.crafting.min_per_hour.is_some() {
        println!("Pass an item's count with --count for the same crafting steps");
    }

    if let Some(writer) = &mut csv_writer {
        writer.flush()?;
    }

    Ok(())
}
