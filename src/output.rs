use colored::Colorize;
use num_traits::Zero;
use serde::Serialize;

use std::cmp::Reverse;
use std::collections::HashMap;

use config::CONFIG;
use crafting::Source::{Crafting, TradingPost, Vendor};
use gw2_arbitrage::*;
use item::ITEM_STACK_SIZE;
use money::Money;
use recipe::{Discipline, Recipe};

/// Print detailed information about a profitable item
pub fn print_profitable_item(
    item_ids: &[u32],
    profitable_item: &Result<profit::ProfitableItem, String>,
    api_listings: &HashMap<u32, api::ItemListings>,
    game_data: &crafting::GameData,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = |id: &u32| game_data.item_name(*id);

    let profitable_item = match profitable_item {
        Ok(item) => item,
        Err(stop) => {
            println!("{}", stop);
            return Ok(());
        }
    };

    let crafted_items = &profitable_item.crafted_items;
    let mut sorted = crafted_items.sorted(item_ids, &game_data.recipes);
    let step_groups = steps::discipline_groups(&mut sorted, crafted_items);
    let labels = steps::disciplines_labels(&sorted, &step_groups);

    // in the order they're crafted
    let made = profitable_item.sales.iter().filter(|sale| sale.count > 0);
    let mut sales: Vec<_> = made.cloned().collect();
    sales.sort_by_key(|sale| sorted.iter().rposition(|&(id, ..)| id == sale.id));
    let made: Vec<String> = sales.iter().map(|sale| name(&sale.id)).collect();
    println!("============");
    println!(
        "Shopping list for {} = {} profit ({}, {} / hour, {}%)",
        made.join(", "),
        Money::from_copper(profitable_item.profit.to_copper_value()),
        format_duration((profitable_item.crafting_seconds + profitable_item.market_seconds) as f64),
        profitable_item.profit_per_hour(),
        (profitable_item.profit_on_cost() * 100_f64).round(),
    );
    // the overview here, and the details where each is sold, after its item's last step
    let mut sell_lines = HashMap::new();
    for sale in &sales {
        let price_msg = price_range(sale.max_sell, sale.min_sell);
        // --value replaces the buy orders, so they say nothing about this sale
        let sale_sparkline = match api_listings.get(&sale.id) {
            Some(listings) if CONFIG.crafting.value.is_none() => {
                format!(
                    " {}",
                    trading_post::listing_sparkline(&listings.buys, sale.count)
                )
            }
            _ => String::new(),
        };
        let sell = format!(
            "Sell {} x {} at: {}{}",
            sale.count,
            name(&sale.id),
            price_msg,
            sale_sparkline
        );
        let profit = Money::from_copper(sale.profit.to_copper_value());
        println!("{}, Profit: {}", sell, profit);
        let line = format!("{}, Breakeven price: {}", sell, sale.breakeven);
        let last_step = sorted.iter().rposition(|&(id, ..)| id == sale.id);
        sell_lines.insert(last_step.expect("Missing step for a sale"), line);
    }
    match profitable_item.payout {
        Some(payout) => println!(
            "Money Required: {}, TP pays out: {}",
            profitable_item.money_required, payout
        ),
        None => println!("Money Required: {}", profitable_item.money_required),
    }

    let bought = steps::bought_steps(&sorted);
    println!("============");
    let inventory = print_shopping_list(profitable_item, &sorted, &bought, api_listings, game_data)
        + crafted_items.mystic_forge_splits(&game_data.recipes);

    println!("============");
    println!("Max inventory slots: {}", inventory + 1); // + 1 for the crafting output
    for sale in &sales {
        println!(
            "Crafting steps: https://gw2efficiency.com/crafting/calculator/a~1!b~1!c~1!d~{}-{}",
            sale.count, sale.id
        );
    }
    print_crafting_steps(
        &sorted,
        &step_groups,
        &labels,
        &bought,
        &sell_lines,
        game_data,
    );

    // nothing unlocks these, or nothing that does could be bought or crafted
    let unpriced: Vec<String> = sorted
        .iter()
        .filter(|(.., recipe)| {
            recipe
                .id
                .is_some_and(|id| crafted_items.unlocks.get(&id) == Some(&None))
        })
        .map(|&(id, .., recipe)| format!("{} (recipe {})", name(&id), recipe.id.unwrap()))
        .collect();
    let unmade = || profitable_item.sales.iter().filter(|sale| sale.count == 0);
    if !unpriced.is_empty() || !crafted_items.leftovers.is_empty() || unmade().next().is_some() {
        println!("============");
    }
    if !unpriced.is_empty() {
        println!(
            "Not in the cost, as nothing that unlocks them could be priced; learn these recipes first: {}",
            unpriced.join(", ")
        );
    }

    print_leftovers(profitable_item, game_data);
    for sale in unmade() {
        println!("Not made: {}: {}", name(&sale.id), sale.stop);
    }

    Ok(())
}

/// What's left over, with what it sells for, after the profit their sale adds
fn print_leftovers(profitable_item: &profit::ProfitableItem, game_data: &crafting::GameData) {
    let leftovers = &profitable_item.crafted_items.leftovers;
    if leftovers.is_empty() {
        return;
    }
    let sold: Money = profitable_item.sales.iter().map(|sale| sale.profit).sum();
    match (profitable_item.profit - sold).to_copper_value() {
        0 => println!("Leftovers:"),
        profit => println!("Leftovers: {} profit", Money::from_copper(profit)),
    }
    let mut leftovers: Vec<_> = leftovers.iter().collect();
    leftovers.sort_unstable_by_key(|&(&id, _)| id);
    for (&leftover_id, &(count, ..)) in leftovers {
        let (on_tp, sells, lists, breakeven) = profitable_item.leftover_sales[&leftover_id];
        // after fees: sold into the buy orders now, or listed at the lowest sell
        let market = match (sells, lists) {
            (Some(sells), Some(lists)) => format!(", sells for {} to {} each", sells, lists),
            (Some(sells), None) => format!(", sells for {} each", sells),
            (None, Some(lists)) => format!(", lists for {} each", lists),
            (None, None) => String::new(),
        };
        let item = game_data.items.get(&leftover_id);
        let vendor = item.map_or(Money::zero(), |item| item.vendor_sale_value());
        let vendored = match count - on_tp {
            0 => String::new(),
            _ if vendor.is_zero() => String::new(),
            vendored => format!(", vendor {} for {} each", vendored, vendor),
        };
        println!(
            "{} {}, breakeven: {} each{}{}",
            count,
            game_data.item_name(leftover_id),
            breakeven,
            market,
            vendored,
        );
    }
}

/// The purchases, TP before vendor, then the merchant steps bought with them; returns the inventory
/// slots they take
fn print_shopping_list(
    profitable_item: &profit::ProfitableItem,
    steps: &[(u32, u32, &Recipe)],
    bought: &[bool],
    api_listings: &HashMap<u32, api::ItemListings>,
    game_data: &crafting::GameData,
) -> u32 {
    let name = |id: &u32| game_data.item_name(*id);
    let mut sorted_ingredients: Vec<(&(u32, crafting::Source), &crafting::PurchasedIngredient)> =
        profitable_item.purchases.iter().collect();
    // vendor items after trading post ones; then most bought first, then highest cost and id
    sorted_ingredients.sort_unstable_by_key(|&(&(id, source), i)| {
        (source == Vendor, Reverse((i.count, i.total_cost, id)))
    });

    let mut inventory = 0;
    for (&(ingredient_id, source), ingredient) in sorted_ingredients {
        let (purchase_count, source_msg) = match source {
            TradingPost => {
                let sparkline = api_listings
                    .get(&ingredient_id)
                    .map_or_else(String::new, |listings| {
                        trading_post::listing_sparkline(&listings.sells, ingredient.count)
                    });
                let price = price_range(ingredient.min_price, ingredient.max_price);
                let subtotal = ingredient.total_cost;
                let msg = format!(" (at {}) Subtotal: {} {}", price, subtotal, sparkline);
                // equal stacks when the count allows, without another transaction
                let stacks = ingredient.count.div_ceil(ITEM_STACK_SIZE);
                match ingredient.count % stacks {
                    0 => (ingredient.count / stacks, msg),
                    _ => (ITEM_STACK_SIZE, msg),
                }
            }
            Vendor => {
                let item = game_data.items.get(&ingredient_id);
                let missing = || panic!("Missing item for ingredient {}", ingredient_id);
                let item = item.unwrap_or_else(missing);
                let subtotal = ingredient.total_cost;
                match item.vendor_lot() {
                    Some((cost, lot)) if lot > 1 => {
                        let msg = format!(" (vendor: {} per {}) Subtotal: {}", cost, lot, subtotal);
                        (lot, msg)
                    }
                    Some((cost, lot)) => {
                        (lot, format!(" (vendor: {}) Subtotal: {}", cost, subtotal))
                    }
                    None => (1, String::new()),
                }
            }
            Crafting => (ITEM_STACK_SIZE, String::new()),
        };
        let (count, slots) = count_in_lots(ingredient.count, purchase_count);
        inventory += slots;
        println!("{} {}{}", count, name(&ingredient_id), source_msg);
    }

    let vendor_steps = steps.iter().zip(bought).filter(|(_, &bought)| bought);
    for (&(item_id, count, recipe), _) in vendor_steps {
        let lot = recipe.output_item_count;
        let per_lot = if lot > 1 {
            format!(" per {}", lot)
        } else {
            "".to_string()
        };
        let (count_msg, slots) = count_in_lots(count, lot);
        inventory += slots;
        println!(
            "{} {} (vendor: {}{}) Subtotal: {}",
            count_msg,
            name(&item_id),
            recipe_price(recipe, 1, game_data),
            per_lot,
            recipe_price(recipe, count / lot, game_data),
        );
    }
    inventory
}

/// `count`, and how many lots of `lot` it is when more than one, e.g. "520 (2 x 250 + 20)"; and the
/// inventory slots it takes
fn count_in_lots(count: u32, lot: u32) -> (String, u32) {
    if lot > 1 && count > lot {
        let remainder = match count % lot {
            0 => String::new(),
            remainder => format!(" + {}", remainder),
        };
        let msg = format!("{} ({} x {}{})", count, count / lot, lot, remainder);
        (msg, count.div_ceil(ITEM_STACK_SIZE))
    } else {
        (count.to_string(), 1)
    }
}

/// The steps not bought, under the stations they're crafted at, each sale after its item's last step
fn print_crafting_steps(
    steps: &[(u32, u32, &Recipe)],
    step_groups: &[Option<usize>],
    labels: &[String],
    bought: &[bool],
    sell_lines: &HashMap<usize, String>, // step index, the sale made after it
    game_data: &crafting::GameData,
) {
    use Discipline::{DoubleClick, Merchant, Salvage};
    let print_sale = |index| {
        if let Some(line) = sell_lines.get(&index) {
            println!("{}", line);
        }
    };
    let mut current = None;
    let steps = steps.iter().zip(step_groups).zip(bought).enumerate();
    for (index, ((&(item_id, count, recipe), &group), &bought)) in steps {
        if bought {
            print_sale(index);
            continue;
        }
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
        } else if recipe.disciplines.contains(&Merchant) {
            "Buy "
        } else {
            ""
        };
        let num_crafted = count / recipe.output_item_count;
        let item_name = game_data.item_name(item_id);
        let ingredients = recipe_price(recipe, num_crafted, game_data);
        if recipe.output_item_count > 1 {
            println!(
                "{}{} (makes {}) {} from {}",
                action, num_crafted, count, item_name, ingredients
            );
        } else {
            println!("{}{} {} from {}", action, count, item_name, ingredients);
        }
        print_sale(index);
    }
}

/// A recipe's ingredients and currencies for `batches`
fn recipe_price(recipe: &Recipe, batches: u32, game_data: &crafting::GameData) -> String {
    let mut ingredients = recipe
        .sorted_ingredients()
        .iter()
        .map(|ingredient| {
            let ingredient_name = game_data.item_name(ingredient.item_id);
            format!("{} {}", ingredient.count * batches, ingredient_name)
        })
        .collect::<Vec<String>>();
    if !recipe.currencies.is_empty() {
        if let Some(cost) = recipe.currency_cost() {
            ingredients.push((cost * batches).to_string());
        }
    }
    ingredients.join(" ")
}

/// Prices from the first filled to the last
fn price_range(first: Money, last: Money) -> String {
    match first == last {
        true => first.to_string(),
        false => format!("{} to {}", first, last),
    }
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
pub fn print_item_list(
    profitable_items: &[profit::ProfitableItem],
    game_data: &crafting::GameData,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut csv_writer = if let Some(path) = &CONFIG.output_csv {
        Some(csv::Writer::from_path(path)?)
    } else {
        None
    };

    // (heading, align left)
    let columns = [
        ("Name", true),
        ("Disciplines", true),
        ("Item id", false),
        ("Recipes", false),
        ("Total profit", true),
        ("Count", false),
        ("Unit", false),
        ("Time", false),
        ("Profit / hr", false),
        ("ROI", false),
    ];

    // Total profit pads coin and other currencies separately, so the gold lines up
    let totals: Vec<(String, String)> = profitable_items
        .iter()
        .map(|item| {
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
    for (profitable_item, (coin, rest)) in profitable_items.iter().zip(totals) {
        let unlocks = &profitable_item.crafted_items.unlocks;
        // Only required when prices are cached.
        // Profit may end up being 0, since potential profitable items are selected based
        // on cached prices, but the actual profit is calculated using detailed listings and
        // prices may have changed since they were cached.
        if profitable_item.count() == 0 {
            continue;
        }

        let item_id = profitable_item.sales[0].id;
        let name = game_data.item_name(item_id);

        let profit_per_hour = profitable_item.profit_per_hour();

        // e.g. "MF+Sr 400+J 400"
        let crafted_items = &profitable_item.crafted_items;
        let mut sorted = crafted_items.sorted(&[item_id], &game_data.recipes);
        let step_groups = steps::discipline_groups(&mut sorted, crafted_items);
        let mut labels = steps::disciplines_labels(&sorted, &step_groups);
        if labels.is_empty() {
            let &(.., recipe) = sorted.last().expect("Missing recipe");
            let abbrevs: Vec<String> = recipe.disciplines.iter().map(|d| d.get_abbrev()).collect();
            labels.push(abbrevs.join("/"));
        }

        let output_row = OutputRow {
            name: name.to_string(),
            disciplines: labels.join("+"),
            item_id,
            unknown_recipes: unlocks.keys().copied().collect(),
            total_profit: profitable_item.profit.to_string(),
            number_required: profitable_item.count(),
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
            unlocks
                .iter()
                .map(|(recipe_id, unlock)| match unlock {
                    Some((_, cost)) => format!("{} ({})", recipe_id, cost),
                    None => format!("{} (?)", recipe_id),
                })
                .collect::<Vec<String>>()
                .join(","),
            format!("{:>coin_width$}{:<rest_width$}", coin, rest),
            output_row.number_required.to_string(),
            Money::from_copper(output_row.profit_per_item).to_string(),
            format_duration(output_row.crafting_seconds + output_row.market_seconds),
            format!("{}/hr", profit_per_hour),
            format!("{}%", (output_row.profit_on_cost * 100_f64).round()),
        ]);
    }
    print_table(columns, &rows);

    let total_profit: Money = profitable_items.iter().map(|item| item.profit).sum();
    println!("Total: {}", total_profit);
    if CONFIG.crafting.min_per_hour.is_some() {
        println!("Pass the same --min-per-hour with an item id for the same crafting steps");
    }

    if let Some(writer) = &mut csv_writer {
        writer.flush()?;
    }

    Ok(())
}

/// `rows` in columns as wide as their widest cell, each row in the next color, between the
/// headings at top and bottom
fn print_table<const N: usize>(columns: [(&str, bool); N], rows: &[[String; N]]) {
    let mut line_colors = [
        colored::Color::Red,
        colored::Color::Green,
        colored::Color::Yellow,
        colored::Color::Magenta,
        colored::Color::Cyan,
    ]
    .iter()
    .cycle();

    let header = columns.map(|(heading, _)| heading.to_string());
    let mut widths = header.each_ref().map(|cell| cell.chars().count());
    for row in rows {
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
    for row in rows {
        println!("{}", format_row(row).color(*line_colors.next().unwrap()));
    }
    println!("{}", rule);
    println!("{}", header);
    println!("{}", rule);
}
