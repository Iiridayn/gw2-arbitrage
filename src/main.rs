use colored::Colorize;
use serde::Serialize;

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io;
use std::io::prelude::*;
use std::time::Instant;

use config::CONFIG;
use gw2_arbitrage::*;
use item::{Item, ITEM_STACK_SIZE};
use money::Money;
use recipe::{Discipline, Recipe};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let notify_print = |url: &str| println!("Fetching {}", url);
    let notify = Some(&notify_print as &dyn Fn(&str));

    // Without an API key, assume we know none
    let known_recipes = if let Some(key) = &CONFIG.api_key {
        request::fetch_account_recipes(key, &CONFIG.cache_dir, notify)
            .await
            .unwrap_or_else(|error| {
                eprintln!("API error fetching recipe unlocks: {}", error);
                HashSet::new()
            })
    } else {
        HashSet::new()
    };

    let mut stage = Instant::now();
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
        api_recipes.retain(|recipe| !recipe.disciplines.is_empty());
        api_recipes
    };
    println!(
        "Loaded {} recipes stored at '{}' in {} ms",
        api_recipes.len(),
        CONFIG.api_recipes_file.display(),
        lap_ms(&mut stage)
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
        "Loaded {} custom recipes stored at '{}' in {} ms",
        custom_recipes.len(),
        CONFIG.custom_recipes_file.display(),
        lap_ms(&mut stage)
    );

    println!("Loading items");
    let items: Vec<Item> =
        request::get_data(&CONFIG.items_file, request::ITEMS_MIN_TIMESTAMP, || async {
            let api_items: Vec<api::ApiItem> =
                request::request_paginated("items", &CONFIG.lang, notify).await?;
            Ok(api_items.into_iter().map(Item::from).collect())
        })
        .await?;
    println!(
        "Loaded {} items stored at '{}' in {} ms",
        items.len(),
        CONFIG.items_file.display(),
        lap_ms(&mut stage)
    );

    let recipes = api_recipes
        .into_iter()
        .map(Recipe::from)
        // api recipes first, so they're kept over custom recipes that make the same
        .chain(custom_recipes)
        .filter(|recipe| {
            // Profit can't be calculated unless all of the recipe's currencies have a copper value
            if recipe.currency_cost().is_none() {
                return false;
            }
            if !CONFIG.include_mystic_forge && recipe.disciplines.contains(&Discipline::MysticForge)
            {
                return false;
            }
            if !CONFIG.include_unknown_recipes
                && !recipe.is_automatic()
                && recipe.id.is_some_and(|id| !known_recipes.contains(&id))
            {
                return false;
            }
            if let Some(output_blacklist) = &CONFIG.output_blacklist {
                if output_blacklist.contains(&recipe.output_item_id) {
                    return false;
                }
            }
            if let Some(input_blacklist) = &CONFIG.input_blacklist {
                for ingredient in &recipe.ingredients {
                    if input_blacklist.contains(&ingredient.item_id) {
                        return false;
                    }
                }
            }

            true
        });
    let game_data = crafting::GameData::new(recipes, items);

    let unlocks = crafting::RecipeUnlocks::new(&game_data.items, &known_recipes);

    let item_id = match &CONFIG.name {
        Some(query) => match find_item_by_name(query, &game_data) {
            Some(item_id) => Some(item_id),
            None => return Ok(()),
        },
        None => CONFIG.item_id,
    };

    if let Some(item_id) = item_id {
        let (profitable_item, purchased_ingredients, tp_listings_map) =
            profit::calc_item_profit(item_id, &game_data, &unlocks, notify).await?;
        print_profitable_item(
            item_id,
            &profitable_item,
            &purchased_ingredients,
            &tp_listings_map,
            &game_data,
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
        let tp_prices = request::fetch_tp_prices(Some(&commerce_notify as &dyn Fn(&str))).await?;
        println!();
        println!(
            "Loaded {} trading post prices in {} ms",
            tp_prices.len(),
            lap_ms(&mut stage)
        );

        let prices =
            crafting::SummaryPrices::new(profit::vec_to_map(tp_prices, |x| x.id), &game_data);
        println!(
            "Found the lowest possible cost of {} items in {} ms",
            prices.lower_bounds.len(),
            lap_ms(&mut stage)
        );

        let (profitable_item_ids, ingredient_ids) =
            profit::find_profitable_items(&prices, &game_data);
        println!(
            "Found {} candidates in {} ms",
            profitable_item_ids.len(),
            lap_ms(&mut stage)
        );

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
            "Loaded {} detailed trading post listings in {} ms",
            tp_listings.len(),
            lap_ms(&mut stage)
        );
        let tp_listings_map = profit::vec_to_map(tp_listings, |x| x.id);

        let profitable_items = profit::profitable_item_list(
            &tp_listings_map,
            &profitable_item_ids,
            &request_listing_item_ids,
            &game_data,
            &prices,
            &unlocks,
        );
        println!(
            "Priced {} items in {} ms",
            profitable_items.len(),
            lap_ms(&mut stage)
        );

        print_item_list(&profitable_items, &game_data)?;
    }

    Ok(())
}

/// Milliseconds since `start`, restarting it
fn lap_ms(start: &mut Instant) -> u128 {
    let ms = start.elapsed().as_millis();
    *start = Instant::now();
    ms
}

/// Find the craftable item with a word starting with each query word, in any order. An exact name
/// wins over longer ones; prints the candidates or why there are none, if not exactly one.
fn find_item_by_name(query: &str, game_data: &crafting::GameData) -> Option<u32> {
    let query = query.to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut matches: Vec<(&Item, String)> = game_data
        .recipes
        .keys()
        .filter_map(|id| game_data.items.get(id))
        .map(|item| (item, item.name.to_lowercase()))
        .filter(|(_, name)| {
            words
                .iter()
                .all(|word| name.split_whitespace().any(|n| n.starts_with(word)))
        })
        .collect();

    let exact: Vec<u32> = matches
        .iter()
        .filter(|(_, name)| *name == words.join(" "))
        .map(|(item, _)| item.id)
        .collect();
    if let [item_id] = exact[..] {
        return Some(item_id);
    }
    match matches.len() {
        0 => println!("No craftable item matches; check -m, -u and the blacklists"),
        1 => return Some(matches[0].0.id),
        count => {
            const SHOWN: usize = 20;
            println!("{} items match; pass an item id instead:", count);
            matches.sort_unstable_by(|(_, a), (_, b)| a.cmp(b));
            for (item, _) in matches.iter().take(SHOWN) {
                println!("{:>8} {}", item.id, item);
            }
            if count > SHOWN {
                println!("...and {} more", count - SHOWN);
            }
        }
    }
    None
}

/// Print detailed information about a profitable item
fn print_profitable_item(
    item_id: u32,
    profitable_item: &Result<(profit::ProfitableItem, crafting::RequiredUnlocks), String>,
    purchased_ingredients: &HashMap<(u32, crafting::Source), crafting::PurchasedIngredient>,
    tp_listings_map: &HashMap<u32, api::ItemListings>,
    game_data: &crafting::GameData,
) -> Result<(), Box<dyn std::error::Error>> {
    let (profitable_item, required_unlocks) = match profitable_item {
        Ok(item) => item,
        Err(stop) => {
            if !game_data.recipes.contains_key(&item_id) {
                println!("No usable recipe; check -m, -u and the blacklists");
            } else if CONFIG.crafting.value.is_none()
                && tp_listings_map
                    .get(&item_id)
                    .is_none_or(|listings| listings.buys.is_empty())
            {
                println!("No buy orders on the trading post; pass --value to price it");
            } else {
                println!("{}", stop);
            }
            return Ok(());
        }
    };

    let name = |id: &u32| {
        let item = game_data.items.get(id);
        item.map_or_else(|| "???".to_string(), |item| item.to_string())
    };

    println!("============");
    println!(
        "Shopping list for {} x {} = {} profit ({}, {} / hour, {}%)",
        profitable_item.count,
        name(&item_id),
        Money::from_copper(profitable_item.profit.to_copper_value()),
        format_duration((profitable_item.crafting_seconds + profitable_item.market_seconds) as f64),
        profitable_item.profit_per_hour(),
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
            trading_post::listing_sparkline(&listings.buys, profitable_item.count)
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

    let mut sorted = profitable_item
        .crafted_items
        .sorted(item_id, &game_data.recipes);
    let step_groups = crafting::discipline_groups(&mut sorted);
    let labels = crafting::disciplines_labels(&sorted, &step_groups);

    // vendor recipes are bought with the rest, unless they take something crafted first
    let mut crafted = HashSet::new();
    let bought: Vec<bool> = sorted
        .iter()
        .map(|&(id, _, recipe)| {
            let uses_crafted = recipe
                .ingredients
                .iter()
                .any(|i| crafted.contains(&i.item_id));
            let bought = recipe.disciplines.contains(&Discipline::Merchant) && !uses_crafted;
            if !bought {
                crafted.insert(id);
            }
            bought
        })
        .collect();

    // a recipe's ingredients and currencies for `batches`
    let recipe_price = |recipe: &Recipe, batches: u32| {
        let mut ingredients = recipe
            .sorted_ingredients()
            .iter()
            .map(|ingredient| {
                let ingredient_name = name(&ingredient.item_id);
                format!("{} {}", ingredient.count * batches, ingredient_name)
            })
            .collect::<Vec<String>>();
        if !recipe.currencies.is_empty() {
            if let Some(cost) = recipe.currency_cost() {
                ingredients.push((cost * batches).to_string());
            }
        }
        ingredients.join(" ")
    };

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
    let mut count_msg = |count: u32, purchase_count: u32| {
        if purchase_count > 1 && count > purchase_count {
            let stack_count = count / purchase_count;
            inventory += count.div_ceil(ITEM_STACK_SIZE);
            let remainder = count % purchase_count;
            let remainder_msg = if remainder != 0 {
                format!(" + {}", remainder)
            } else {
                "".to_string()
            };
            format!(
                "{} ({} x {}{})",
                count, stack_count, purchase_count, remainder_msg
            )
        } else {
            inventory += 1;
            count.to_string()
        }
    };

    for ((ingredient_id, ingredient_source), ingredient) in sorted_ingredients {
        let purchase_count = if *ingredient_source == crafting::Source::Vendor {
            game_data
                .items
                .get(ingredient_id)
                .unwrap_or_else(|| panic!("Missing item for ingredient {}", ingredient_id))
                .vendor_cost()
                .unwrap_or((Money::from_copper(0), 1))
                .1
        } else {
            ITEM_STACK_SIZE
        };
        let ingredient_count_msg = count_msg(ingredient.count, purchase_count);
        let source_msg = match *ingredient_source {
            crafting::Source::TradingPost => {
                let sparkline = tp_listings_map
                    .get(ingredient_id)
                    .map_or_else(String::new, |listings| {
                        trading_post::listing_sparkline(&listings.sells, ingredient.count)
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
                let vendor_cost = game_data
                    .items
                    .get(ingredient_id)
                    .unwrap_or_else(|| panic!("Missing item for ingredient {}", ingredient_id))
                    .vendor_cost();
                if let Some((cost, purchase_count)) = vendor_cost {
                    if purchase_count > 1 {
                        format!(
                            " (vendor: {} per {}) Subtotal: {}",
                            cost,
                            purchase_count,
                            cost * ingredient.count.div_ceil(purchase_count),
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
            name(ingredient_id),
            source_msg,
        );
    }

    let vendor_steps = sorted.iter().zip(&bought).filter(|(_, &bought)| bought);
    for (&(item_id, count, recipe), _) in vendor_steps {
        let lot = recipe.output_item_count;
        let per_lot = if lot > 1 {
            format!(" per {}", lot)
        } else {
            "".to_string()
        };
        println!(
            "{} {} (vendor: {}{}) Subtotal: {}",
            count_msg(count, lot),
            name(&item_id),
            recipe_price(recipe, 1),
            per_lot,
            recipe_price(recipe, count / lot),
        );
    }

    // Mystic Forge recipes with an ingredient repeated need its stack split, once for all of them
    let mut splits: HashMap<u32, usize> = HashMap::new();
    for (id, index) in profitable_item.crafted_items.crafted.keys() {
        let recipe = &game_data.recipes[id][*index];
        if !recipe.disciplines.contains(&Discipline::MysticForge) {
            continue;
        }
        for ingredient in &recipe.ingredients {
            let copies = recipe
                .ingredients
                .iter()
                .filter(|i| i.item_id == ingredient.item_id);
            let split = splits.entry(ingredient.item_id).or_default();
            *split = (*split).max(copies.count() - 1);
        }
    }
    inventory += splits.values().sum::<usize>() as u32;

    println!("============");
    println!("Max inventory slots: {}", inventory + 1); // + 1 for the crafting output
    println!(
        "Crafting steps: https://gw2efficiency.com/crafting/calculator/a~1!b~1!c~1!d~{}-{}",
        profitable_item.count, item_id
    );
    use Discipline::{DoubleClick, Salvage};
    let mut current = None;
    let steps = sorted.iter().zip(&step_groups).zip(&bought);
    for ((&(item_id, count, recipe), &group), _) in steps.filter(|(_, &bought)| !bought) {
        if let Some(group) = group {
            let label = &labels[group];
            match current {
                Some(c) if c != group => println!("Switch to {}:", label),
                Some(_) => (),
                None => println!("On {}:", label),
            }
            current = Some(group);
        }
        let action = if recipe.disciplines.contains(&Salvage) {
            "Salvage "
        } else if recipe.disciplines.contains(&DoubleClick) {
            "Double click "
        } else {
            ""
        };
        let num_crafted = count / recipe.output_item_count;
        let item_name = name(&item_id);
        let ingredients = recipe_price(recipe, num_crafted);
        if recipe.output_item_count > 1 {
            println!(
                "{}{} (makes {}) {} from {}",
                action, num_crafted, count, item_name, ingredients
            );
        } else {
            println!("{}{} {} from {}", action, count, item_name, ingredients);
        }
    }

    if !required_unlocks.is_empty() {
        let req_recipes = required_unlocks
            .iter()
            .map(|(recipe_id, unlock)| match unlock {
                Some((unlock_id, cost)) => format!("{}, {}", name(unlock_id), cost),
                // recipe 5424 for item 29407 has no unlock item, possibly others
                None => format!("Recipe {} has no unlock item with a price", recipe_id),
            })
            .collect::<Vec<String>>()
            .join("\n");
        println!(
            "You {} craft this yet. Required recipes{}:\n{}",
            if CONFIG.api_key.is_some() {
                "can not"
            } else {
                "may not be able to"
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
                name(leftover_id),
                (*cost / *count).trading_post_listing_price(),
            );
        }
    }

    Ok(())
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
    profitable_items: &[(profit::ProfitableItem, crafting::RequiredUnlocks)],
    game_data: &crafting::GameData,
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

    // (heading, align left)
    let columns = [
        ("Name", true),
        ("Disciplines", true),
        ("Item id", false),
        ("Recipes", false),
        ("Total profit", true),
        ("Count", false),
        // ("Profit / item", false),
        ("Time", false),
        ("Profit / hr", false),
        ("ROI", false),
    ];
    let header = columns.map(|(heading, _)| heading.to_string());

    // Total profit pads coin and other currencies separately, so the gold lines up
    let totals: Vec<(String, String)> = profitable_items
        .iter()
        .map(|(item, _)| {
            let (coin, currencies) = item.profit.split_display();
            let sep = match (coin.is_empty(), currencies.is_empty()) {
                (_, true) => "",
                (true, false) => "  ",
                (false, false) => ", ",
            };
            (coin, format!("{}{}", sep, currencies))
        })
        .collect();
    let coin_width = totals.iter().map(|(coin, _)| coin.len()).max().unwrap_or(0);
    let rest_width = totals.iter().map(|(_, rest)| rest.len()).max().unwrap_or(0);

    let mut rows = vec![];
    for ((profitable_item, required_unlocks), (coin, rest)) in profitable_items.iter().zip(totals) {
        // Only required when prices are cached.
        // Profit may end up being 0, since potential profitable items are selected based
        // on cached prices, but the actual profit is calculated using detailed listings and
        // prices may have changed since they were cached.
        if profitable_item.count == 0 {
            continue;
        }

        let item_id = profitable_item.id;
        let name = game_data
            .items
            .get(&item_id)
            .map_or_else(|| "???".to_string(), |item| item.to_string());

        let profit_per_hour = profitable_item.profit_per_hour();

        // e.g. "MF+Sr 400+J 400"
        let mut sorted = profitable_item
            .crafted_items
            .sorted(item_id, &game_data.recipes);
        let step_groups = crafting::discipline_groups(&mut sorted);
        let mut labels = crafting::disciplines_labels(&sorted, &step_groups);
        if labels.is_empty() {
            let recipe = sorted.last().expect("Missing recipe").2;
            let abbrevs: Vec<String> = recipe.disciplines.iter().map(|d| d.get_abbrev()).collect();
            labels.push(abbrevs.join("/"));
        }

        let output_row = OutputRow {
            name: name.to_string(),
            disciplines: labels.join("+"),
            item_id,
            unknown_recipes: required_unlocks.keys().copied().collect(),
            total_profit: profitable_item.profit.to_string(),
            number_required: profitable_item.count,
            profit_per_item: profitable_item.profit_per_item().to_copper_value(),
            crafting_steps: profitable_item.crafting_steps,
            profit_per_step: profitable_item.profit_per_crafting_step().to_copper_value(),
            crafting_seconds: profitable_item.crafting_seconds as f64,
            market_seconds: profitable_item.market_seconds as f64,
            profit_per_hour: profit_per_hour.to_copper_value(),
            profit_on_cost: profitable_item.profit_on_cost(),
        };

        if let Some(writer) = &mut csv_writer {
            writer.serialize(&output_row)?;
        }

        rows.push([
            output_row.name,
            output_row.disciplines,
            output_row.item_id.to_string(),
            required_unlocks
                .iter()
                .map(|(recipe_id, unlock)| match unlock {
                    Some((_, cost)) => format!("{} ({})", recipe_id, cost),
                    None => format!("{} (?)", recipe_id),
                })
                .collect::<Vec<String>>()
                .join(","),
            format!("{:>coin_width$}{:<rest_width$}", coin, rest),
            output_row.number_required.to_string(),
            // Money::from_copper(output_row.profit_per_item).to_string(),
            format_duration(output_row.crafting_seconds + output_row.market_seconds),
            format!("{}/hr", profit_per_hour),
            format!("{}%", (output_row.profit_on_cost * 100_f64).round()),
        ]);
    }

    let mut widths = header.each_ref().map(|cell| cell.chars().count());
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let format_row = |row: &[String]| {
        row.iter()
            .zip(widths)
            .zip(columns)
            .map(|((cell, w), (_, left))| {
                if left {
                    format!("{:<w$}", cell)
                } else {
                    format!("{:>w$}", cell)
                }
            })
            .collect::<Vec<String>>()
            .join(" ")
    };
    let header = format_row(&header);
    let rule = "=".repeat(header.chars().count());

    println!("{}", header);
    println!("{}", rule);
    for row in &rows {
        println!("{}", format_row(row).color(*line_colors.next().unwrap()));
    }
    println!("{}", rule);
    println!("{}", header);
    println!("{}", rule);

    let total_profit: Money = profitable_items.iter().map(|(item, _)| item.profit).sum();
    println!("Total: {}", total_profit);
    if CONFIG.crafting.min_per_hour.is_some() {
        println!("Pass the same --min-per-hour with an item id for the same crafting steps");
    }

    if let Some(writer) = &mut csv_writer {
        writer.flush()?;
    }

    Ok(())
}
