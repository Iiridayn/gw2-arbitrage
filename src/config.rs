use num_rational::Rational32;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io::Read;
use std::iter::FromIterator;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use once_cell::sync::Lazy;
use serde::Deserialize;
use structopt::StructOpt;
use strum::{EnumString, EnumVariantNames, VariantNames};
use toml;

use lazy_static::lazy_static;

use crate::money::Currency;
use crate::recipe::Discipline;

pub const CACHE_PREFIX: &str = "cache_";

#[derive(Clone, Debug, Default)]
pub struct CraftingOptions {
    pub include_timegated: bool,
    pub count: Option<u32>, // of each item
    pub min_per_item: Option<u32>,
    pub min_per_hour: Option<u32>,
    pub value: Option<u32>,
    pub caps: HashMap<(u32, usize), u32>, // (id, recipe index), most it may make
    pub item_caps: HashMap<u32, u32>, // most of each item asked for; set by the rerun, not a flag
}

#[derive(Default)]
pub struct Config {
    pub crafting: CraftingOptions,

    pub output_csv: Option<PathBuf>,
    pub filter_disciplines: Option<Vec<Discipline>>,
    pub include_mystic_forge: bool,
    pub include_unknown_recipes: bool,
    pub sort: SortBy,
    pub lang: Option<Language>,
    pub api_key: Option<String>,

    // Currency conversion values
    pub ascended: Option<u32>,
    // indexed by `Currency as usize`
    pub rates: [Option<Rational32>; Currency::ALL.len()],

    pub cache_dir: PathBuf,
    pub snapshot_dir: Option<PathBuf>,
    pub api_recipes_file: PathBuf,
    pub custom_recipes_file: PathBuf,
    pub items_file: PathBuf,

    pub input_blacklist: Option<HashSet<u32>>,
    pub output_blacklist: Option<HashSet<u32>>,

    pub item_ids: Vec<u32>,
    pub name: Option<String>,
}

lazy_static! {
    pub static ref CONFIG: Config = Config::new();
}

impl Config {
    fn new() -> Self {
        let mut config = Config::default();

        let opt = Opt::from_args();

        config.crafting.count = opt.count;
        config.crafting.min_per_item = opt.min_per_item;
        config.crafting.min_per_hour = opt.min_per_hour;
        config.crafting.value = opt.value;

        config.output_csv = opt.output_csv;

        let mut seen = HashSet::new();
        config.item_ids = opt
            .item_ids
            .into_iter()
            .filter(|&id| seen.insert(id))
            .collect();
        config.name = opt.name;

        config.filter_disciplines = opt.filter_disciplines;

        config.sort = opt.sort.unwrap_or(if opt.min_per_hour.is_some() {
            SortBy::Hour
        } else {
            SortBy::Total
        });

        let file: ConfigFile = match get_file_config(&opt.config_file) {
            Ok(config) => config,
            Err(e) => {
                println!("Error opening config file: {}", e);
                ConfigFile::default()
            }
        };

        config.api_key = file.api_key;

        config.crafting.include_timegated =
            opt.include_timegated || file.include_timegated.unwrap_or(false);
        config.include_mystic_forge =
            opt.include_mystic_forge || file.include_mystic_forge.unwrap_or(false);
        config.include_unknown_recipes =
            opt.include_unknown_recipes || file.include_unknown_recipes.unwrap_or(false);

        config.lang = if opt.lang.is_some() {
            opt.lang
        } else if let Some(code) = file.lang {
            code.parse().map_or_else(
                |e| {
                    println!("Config file: {}", e);
                    None
                },
                Some,
            )
        } else {
            None
        };

        config.ascended = if let Some(provided) = opt.ascended_value {
            provided.or(Some(0))
        } else if let Some(currencies) = &file.currencies {
            currencies.ascended
        } else {
            None
        };

        let currencies = file.currencies.as_ref();
        for &currency in Currency::ALL.iter() {
            let (opt_rate, file_rate) = match currency {
                Currency::Karma => (opt.karma, currencies.and_then(|c| c.karma)),
                Currency::UnboundMagic => (opt.um, currencies.and_then(|c| c.um)),
                Currency::VolatileMagic => (opt.vm, currencies.and_then(|c| c.vm)),
                Currency::ResearchNote => (opt.rn, currencies.and_then(|c| c.rn)),
                Currency::SpiritShard => (opt.ss, currencies.and_then(|c| c.ss)),
            };
            config.rates[currency as usize] = opt_rate
                .or(file_rate)
                .and_then(Rational32::approximate_float);
        }

        if let Some(blacklists) = file.blacklist {
            config.input_blacklist = blacklists.inputs.map(HashSet::from_iter);
            config.output_blacklist = blacklists.outputs.map(HashSet::from_iter);
        };

        let cache_dir = cache_dir(&opt.cache_dir).expect("Failed to identify cache dir");
        ensure_dir(&cache_dir).expect("Failed to create cache dir");
        if let Err(e) = flush_cache(&cache_dir) {
            println!("Failed to flush cache dir {}: {}", &cache_dir.display(), e)
        }
        config.cache_dir = cache_dir;
        if let Some(dir) = &opt.snapshot {
            ensure_dir(dir).expect("Failed to create snapshot dir");
        }
        config.snapshot_dir = opt.snapshot;

        let data_dir = data_dir(&opt.data_dir).expect("Failed to identify data dir");
        ensure_dir(&data_dir).expect("Failed to create data dir");

        let mut api_recipes_path = data_dir.clone();
        api_recipes_path.push("recipes.bin");
        config.api_recipes_file = api_recipes_path;

        let mut custom_recipes_path = data_dir.clone();
        custom_recipes_path.push("custom.bin");
        config.custom_recipes_file = custom_recipes_path;

        let lang_suffix =
            Language::code(&config.lang).map_or_else(|| "".to_string(), |c| format!("_{}", c));
        let mut items_path = data_dir.clone();
        items_path.push(format!("items{}.bin", lang_suffix));
        config.items_file = items_path;

        if opt.reset_data {
            if let Err(e) = remove_data_file(&config.items_file) {
                println!(
                    "Failed to remove file {}: {}",
                    &config.items_file.display(),
                    e
                )
            };
            if let Err(e) = remove_data_file(&config.api_recipes_file) {
                println!(
                    "Failed to remove file {}: {}",
                    &config.api_recipes_file.display(),
                    e
                )
            };
            if let Err(e) = remove_data_file(&config.custom_recipes_file) {
                println!(
                    "Failed to remove file {}: {}",
                    &config.custom_recipes_file.display(),
                    e
                )
            };
        }

        config
    }
}

fn get_file_config(file: &Option<PathBuf>) -> Result<ConfigFile, Box<dyn std::error::Error>> {
    let mut file = File::open(config_file(file)?)?;
    let mut s = String::new();
    file.read_to_string(&mut s)?;
    Ok(toml::from_str(&s)?)
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    // API key requires scope unlocks
    api_key: Option<String>,
    lang: Option<String>,
    include_timegated: Option<bool>,
    include_mystic_forge: Option<bool>,
    include_unknown_recipes: Option<bool>,
    currencies: Option<ConfigFileCurrencySection>,
    blacklist: Option<ConfigFileBlacklistSection>,
}
#[derive(Debug, Default, Deserialize)]
struct ConfigFileCurrencySection {
    ascended: Option<u32>,
    karma: Option<f64>,
    um: Option<f64>,
    vm: Option<f64>,
    rn: Option<f64>,
    ss: Option<f64>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFileBlacklistSection {
    inputs: Option<Vec<u32>>,
    outputs: Option<Vec<u32>>,
}

#[derive(StructOpt, Debug)]
struct Opt {
    /// Include timegated recipes such as Deldrimor Steel Ingot
    #[structopt(short = "t", long)]
    include_timegated: bool,

    /// Include Mystic Forge recipes
    #[structopt(short = "m", long)]
    include_mystic_forge: bool,

    /// Include recipes the account hasn't learned, charging the cheapest unlock against profit.
    /// Without an API key, only automatically learned recipes are known
    #[structopt(short = "u", long)]
    include_unknown_recipes: bool,

    /// Output the full list of profitable recipes to this CSV file
    #[structopt(short, long, parse(from_os_str))]
    output_csv: Option<PathBuf>,

    /// Print a shopping list of ingredients for the given item ids, crafted together from one market
    item_ids: Vec<u32>,

    /// Print a shopping list for the item whose name has words starting with each of these, in
    /// any order (e.g. "bo si" for Bolt of Silk)
    #[structopt(long, conflicts_with = "item-ids")]
    name: Option<String>,

    /// Limit the maximum number of items produced for a recipe
    #[structopt(short, long)]
    count: Option<u32>,

    /// Calculate profit based on a fixed value instead of from buy orders
    #[structopt(long)]
    value: Option<u32>,

    /// Minimum profit per item in copper, for every item made
    #[structopt(long)]
    min_per_item: Option<u32>,

    /// Make the count that earns the most over the opportunity cost of your time, in gold per hour
    /// spent crafting and trading
    #[structopt(long)]
    min_per_hour: Option<u32>,

    #[structopt(short = "d", long = "disciplines", use_delimiter = true, help = &DISCIPLINES_HELP, parse(try_from_str = get_discipline))]
    filter_disciplines: Option<Vec<Discipline>>,

    /// Sort the item list by "total" profit, "unit" profit, profit per "hour", or "roi". Defaults
    /// to "hour" with --min-per-hour, otherwise "total"
    #[structopt(long, parse(try_from_str = get_sort))]
    sort: Option<SortBy>,

    /// Download recipes and items from the GW2 API, replacing any previously cached recipes and items
    #[structopt(long)]
    reset_data: bool,

    #[structopt(long, parse(from_os_str), help = &CACHE_DIR_HELP)]
    cache_dir: Option<PathBuf>,

    /// Keep trading post prices and listings in this directory and reuse them on later runs, never
    /// refreshing them; listings not yet in it are fetched and added. For profiling and testing
    /// against a fixed market
    #[structopt(long, parse(from_os_str))]
    snapshot: Option<PathBuf>,

    #[structopt(long, parse(from_os_str), help = &DATA_DIR_HELP)]
    data_dir: Option<PathBuf>,

    #[structopt(long, parse(from_os_str), help = &CONFIG_FILE_HELP)]
    config_file: Option<PathBuf>,

    /// One of "en", "es", "de", or "fr". Defaults to "en"
    // /// One of "en", "es", "de", "fr", or "zh". Defaults to "en"
    #[structopt(long, parse(try_from_str = get_lang))]
    lang: Option<Language>,

    /// Include recipes that require Piles of Bloodstone Dust, Dragonite Ore or Empyreal Fragments,
    /// with an optional opportunity cost per item
    #[structopt(short = "a", long)]
    ascended_value: Option<Option<u32>>,

    /// Include recipes that require ingredients that can only be purchased with karma, using this
    /// conversion factor as the opportunity cost
    #[structopt(long)]
    karma: Option<f64>,

    /// Include recipes that use LW3 map tokens, using this conversion factor as the opportunity cost
    #[structopt(long)]
    um: Option<f64>,

    /// Include recipes that use LW4 map tokens, using this conversion factor as the opportunity cost
    #[structopt(long)]
    vm: Option<f64>,

    /// Include recipes that use research notes, using this conversion factor as the opportunity cost
    #[structopt(long)]
    rn: Option<f64>,

    /// Include recipes that use spirit shards, using this conversion factor as the opportunity cost
    #[structopt(long)]
    ss: Option<f64>,
}

static CACHE_DIR_HELP: Lazy<String> = Lazy::new(|| {
    format!(
        r#"Save cached API calls to this directory

If provided, the parent directory of the cache directory must already exist. Defaults to '{}'."#,
        cache_dir(&None).unwrap().display()
    )
});

static DATA_DIR_HELP: Lazy<String> = Lazy::new(|| {
    format!(
        r#"Save cached recipes and items to this directory

If provided, the parent directory of the cache directory must already exist. Defaults to '{}'."#,
        data_dir(&None).unwrap().display()
    )
});

static CONFIG_FILE_HELP: Lazy<String> = Lazy::new(|| {
    format!(
        r#"Read config options from this file. Supported options:

    api_key = "<key-with-unlocks-scope>"
    lang = "<lang>"
    include_timegated = true
    include_mystic_forge = true
    include_unknown_recipes = true

    [currencies]
    ascended = <opportunity cost per item>
    karma = <opportunity cost per karma>
    um = <opportunity cost per Unbound Magic>
    vm = <opportunity cost per Volatile Magic>
    rn = <opportunity cost per Research Note>
    ss = <opportunity cost per Spirit Shard>

    [blacklist]
    inputs = [<item ids never used as ingredients>]
    outputs = [<item ids never crafted>]

The default file location is '{}'."#,
        config_file(&None).unwrap().display()
    )
});

static DISCIPLINES_HELP: Lazy<String> = Lazy::new(|| {
    format!(
        r#"Only show items craftable by this discipline or comma-separated list of disciplines (e.g. -d=Weaponsmith,Armorsmith)

valid values: {}"#,
        Discipline::VARIANTS.join(", ")
    )
});

#[derive(Copy, Clone, Debug, Default, EnumString, EnumVariantNames)]
#[strum(serialize_all = "lowercase")]
pub enum SortBy {
    #[default]
    Total,
    Hour,
    Roi,
    Unit,
}

fn get_sort<SortBy: FromStr + VariantNames>(
    sort: &str,
) -> Result<SortBy, Box<dyn std::error::Error>> {
    SortBy::from_str(sort).map_err(|_| {
        format!(
            "Invalid sort: {} (valid values are {})",
            sort,
            SortBy::VARIANTS.join(", ")
        )
        .into()
    })
}

#[derive(Debug, EnumString, EnumVariantNames)]
pub enum Language {
    #[strum(serialize = "en")]
    English,
    #[strum(serialize = "es")]
    Spanish,
    #[strum(serialize = "de")]
    German,
    #[strum(serialize = "fr")]
    French,
    // If you read this and can help test the TP code and extract strings from the Chinese version,
    // and would like to see this work in Chinese, please open an issue.
    // #[strum(serialize="zh")]
    // Chinese, // No lang client, and TP might have different data source anyway
}
impl Language {
    pub fn code(lang: &Option<Language>) -> Option<&str> {
        if let Some(lang) = lang {
            match lang {
                Language::English => None, // English is the default, so leave it off
                Language::Spanish => Some("es"),
                Language::German => Some("de"),
                Language::French => Some("fr"),
                //Language::Chinese => Some("zh"),
            }
        } else {
            None
        }
    }
}

fn get_lang<Language: FromStr + VariantNames>(
    code: &str,
) -> Result<Language, Box<dyn std::error::Error>> {
    Language::from_str(code).map_err(|_| {
        format!(
            "Invalid language: {} (valid values are {})",
            code,
            Language::VARIANTS.join(", ")
        )
        .into()
    })
}

fn get_discipline<Discipline: FromStr + VariantNames>(
    discipline: &str,
) -> Result<Discipline, Box<dyn std::error::Error>> {
    Discipline::from_str(discipline).map_err(|_| {
        format!(
            "Invalid discipline: {} (valid values are {})",
            discipline,
            Discipline::VARIANTS.join(", ")
        )
        .into()
    })
}

fn ensure_dir(dir: &PathBuf) -> Result<&PathBuf, Box<dyn std::error::Error>> {
    if !dir.exists() {
        std::fs::create_dir(dir)
            .map_err(|e| format!("Failed to create '{}' ({})", dir.display(), e).into())
            .and(Ok(dir))
    } else {
        Ok(dir)
    }
}

fn cache_dir(dir: &Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(dir) = dir {
        return Ok(dir.clone());
    }
    dirs::cache_dir()
        .filter(|d| d.exists())
        .map(|mut cache_dir| {
            cache_dir.push("gw2-arbitrage");
            cache_dir
        })
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "Failed to access current working directory".into())
}

fn flush_cache(cache_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // Flush cache files older than 30s; enough to re-run an item with a different count. Listings
    // were measured changing every 30-70s, despite max-age=1200.
    // Give a prefix; on Windows the user cache and user local data folders are the same
    let expired = SystemTime::now() - Duration::new(30, 0);
    for file in fs::read_dir(cache_dir)? {
        let file = file?;
        let filename = file.file_name().into_string();
        if let Ok(name) = filename {
            if !name.starts_with(CACHE_PREFIX) {
                continue;
            }
        }
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        if metadata.created()? <= expired {
            fs::remove_file(file.path())?;
        }
    }
    Ok(())
}

fn data_dir(dir: &Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(dir) = dir {
        return Ok(dir.clone());
    }
    dirs::data_dir()
        .filter(|d| d.exists())
        .map(|mut data_dir| {
            data_dir.push("gw2-arbitrage");
            data_dir
        })
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "Failed to access current working directory".into())
}

fn remove_data_file(file: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    if file.exists() {
        println!("Removing existing data file at '{}'", file.display());
        std::fs::remove_file(file)
            .map_err(|e| format!("Failed to remove '{}' ({})", file.display(), e))?;
    }
    Ok(())
}

fn config_file(file: &Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(file) = file {
        return Ok(file.clone());
    }
    dirs::config_dir()
        .filter(|d| d.exists())
        .map(|mut config_dir| {
            config_dir.push("gw2-arbitrage");
            config_dir
        })
        .or_else(|| std::env::current_dir().ok())
        .map(|mut path| {
            path.push("gw2-arbitrage.toml");
            path
        })
        .ok_or_else(|| "Failed to access current working directory".into())
}
