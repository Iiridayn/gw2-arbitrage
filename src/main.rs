use std::collections::HashSet;
use std::io;
use std::io::prelude::*;
use std::time::Instant;

use config::CONFIG;
use gw2_arbitrage::*;
use item::Item;
use recipe::Recipe;

mod output;

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
        .filter(|recipe| recipe.is_usable(&known_recipes));
    let game_data = crafting::GameData::new(recipes, items);

    let item_ids = match &CONFIG.name {
        Some(query) => match find_item_by_name(query, &game_data) {
            Some(item_id) => vec![item_id],
            None => return Ok(()),
        },
        None => CONFIG.item_ids.clone(),
    };

    if !item_ids.is_empty() {
        let (profitable_item, purchased_ingredients, tp_listings_map) =
            profit::calc_item_profit(&item_ids, &game_data, &known_recipes, notify).await?;
        output::print_profitable_item(
            &item_ids,
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

        let profitable_item_ids = profit::find_profitable_items(&prices, &game_data);
        println!(
            "Found {} candidates in {} ms",
            profitable_item_ids.len(),
            lap_ms(&mut stage)
        );

        println!("Loading detailed trading post listings");
        let mut request_listing_item_ids: Vec<u32> = profitable_item_ids
            .iter()
            .flat_map(|&id| game_data.collect_tree_ids(id, Some(&known_recipes)))
            .collect();
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
            &known_recipes,
        );
        println!(
            "Priced {} items in {} ms",
            profitable_items.len(),
            lap_ms(&mut stage)
        );

        output::print_item_list(&profitable_items, &game_data)?;
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
