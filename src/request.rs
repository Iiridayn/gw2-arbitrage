use crate::api::{ItemListings, Price};

use bincode;
use bincode::{deserialize_from, serialize_into};
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use futures::{stream, StreamExt};
use once_cell::sync::Lazy;
use reqwest::header::HeaderMap;
use reqwest::StatusCode;
use serde_json;

use std::cell::Cell;
use std::collections::hash_map::{DefaultHasher, RandomState};
use std::collections::HashSet;
use std::fs::{metadata, File};
use std::future::Future;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use crate::config::{self, CONFIG};

// Data files older than these unix timestamps are re-fetched; bump to refresh existing users
pub const API_RECIPES_MIN_TIMESTAMP: u64 = 1793535619;
pub const CUSTOM_RECIPES_MIN_TIMESTAMP: u64 = 1794467900;
pub const ITEMS_MIN_TIMESTAMP: u64 = 1793535619;

const PARALLEL_REQUESTS: usize = 10;
const MAX_ATTEMPTS: u32 = 4;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
pub const LARGE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PAGE_SIZE: i32 = 200; // https://wiki.guildwars2.com/wiki/API:2#Paging
const MAX_ITEM_ID_LENGTH: i32 = 200; // error returned for greater than this amount

// Shared so requests reuse connections
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .build()
        .expect("Failed to build HTTP client")
});

pub async fn fetch_item_listings(
    item_ids: &[u32],
    cache_dir: Option<&PathBuf>,
    notify: Option<&dyn Fn(&str)>,
) -> Result<Vec<ItemListings>, Box<dyn std::error::Error>> {
    let mut tp_listings: Vec<ItemListings> = match &CONFIG.snapshot_dir {
        Some(dir) => fetch_snapshot_listings(item_ids, &dir.join("listings.bin"), notify).await?,
        None => request_item_ids("commerce/listings", item_ids, cache_dir, notify).await?,
    };

    for listings in &mut tp_listings {
        // by default sells are listed in ascending and buys in descending price.
        // reverse lists to allow best offers to be popped instead of spliced from front.
        listings.buys.reverse();
        listings.sells.reverse();
    }

    Ok(tp_listings)
}

/// Listings kept at `path`, fetching and adding the ids never asked for; ids off the TP have none
async fn fetch_snapshot_listings(
    item_ids: &[u32],
    path: &Path,
    notify: Option<&dyn Fn(&str)>,
) -> Result<Vec<ItemListings>, Box<dyn std::error::Error>> {
    let (mut asked, mut tp_listings): (HashSet<u32>, Vec<ItemListings>) = match File::open(path) {
        Ok(file) => deserialize_from(DeflateDecoder::new(file))?,
        Err(_) => Default::default(),
    };
    let missing: Vec<u32> = item_ids
        .iter()
        .copied()
        .filter(|id| !asked.contains(id))
        .collect();
    if !missing.is_empty() {
        tp_listings.extend(request_item_ids("commerce/listings", &missing, None, notify).await?);
        asked.extend(missing);
        let stream = BufWriter::new(DeflateEncoder::new(
            File::create(path)?,
            Compression::default(),
        ));
        serialize_into(stream, &(asked, &tp_listings))?;
    }
    let item_ids: HashSet<&u32> = item_ids.iter().collect();
    tp_listings.retain(|listings| item_ids.contains(&listings.id));
    Ok(tp_listings)
}

pub async fn fetch_tp_prices(
    notify: Option<&dyn Fn(&str)>,
) -> Result<Vec<Price>, Box<dyn std::error::Error>> {
    let fetch = || request_paginated("commerce/prices", &None, notify);
    match &CONFIG.snapshot_dir {
        Some(dir) => get_data(dir.join("prices.bin"), 0, fetch).await,
        None => fetch().await,
    }
}

pub async fn get_data<T, Fut>(
    data_path: impl AsRef<Path>,
    min_timestamp: u64,
    getter: impl FnOnce() -> Fut,
) -> Result<Vec<T>, Box<dyn std::error::Error>>
where
    T: serde::Serialize,
    T: serde::de::DeserializeOwned,
    Fut: Future<Output = Result<Vec<T>, Box<dyn std::error::Error>>>,
{
    let modified = metadata(&data_path).and_then(|m| m.modified()).ok();
    let min_time = UNIX_EPOCH + Duration::from_secs(min_timestamp);

    if modified.is_none_or(|time| time < min_time) {
        match getter().await {
            Ok(items) => {
                let file = File::create(&data_path)?;
                let stream = BufWriter::new(DeflateEncoder::new(file, Compression::default()));
                serialize_into(stream, &items)?;
                return Ok(items);
            }
            Err(e) if modified.is_none() => return Err(e),
            Err(e) => eprintln!(
                "Failed to refresh '{}', using the existing copy: {}",
                data_path.as_ref().display(),
                e
            ),
        }
    }

    let stream = DeflateDecoder::new(File::open(&data_path)?);
    deserialize_from(stream).map_err(|e| {
        format!(
            "Failed to deserialize existing data at '{}' ({}). \
             Try using the --reset-data flag to replace the data files.",
            data_path.as_ref().display(),
            e,
        )
        .into()
    })
}

pub async fn request_paginated<T>(
    url_path: &str,
    lang: &Option<config::Language>,
    notify: Option<&dyn Fn(&str)>,
) -> Result<Vec<T>, Box<dyn std::error::Error>>
where
    T: serde::Serialize,
    T: serde::de::DeserializeOwned,
{
    let mut page_no = 0;
    let mut page_total = None;

    // update page total with first request
    let mut items: Vec<T> = request_page(url_path, page_no, &mut page_total, lang, notify).await?;

    // fetch remaining pages in parallel batches
    page_no += 1;

    // try fetching one extra page in case page total increased while paginating
    let page_total = page_total.expect("Missing page total") + 1;

    let request_results = stream::iter((page_no..page_total).map(|page_no| async move {
        request_page::<T>(url_path, page_no, &mut Some(page_total), lang, notify).await
    }))
    .buffered(PARALLEL_REQUESTS)
    .collect::<Vec<Result<Vec<T>, Box<dyn std::error::Error>>>>()
    .await;

    for result in request_results.into_iter() {
        let mut new_items = result?;
        items.append(&mut new_items);
    }

    Ok(items)
}

async fn request_page<T>(
    url_path: &str,
    page_no: usize,
    page_total: &mut Option<usize>,
    lang: &Option<config::Language>,
    notify: Option<&dyn Fn(&str)>,
) -> Result<Vec<T>, Box<dyn std::error::Error>>
where
    T: serde::Serialize,
    T: serde::de::DeserializeOwned,
{
    let url = if let Some(code) = config::Language::code(lang) {
        format!(
            "https://api.guildwars2.com/v2/{}?lang={}&page={}&page_size={}",
            url_path, code, page_no, MAX_PAGE_SIZE
        )
    } else {
        format!(
            "https://api.guildwars2.com/v2/{}?page={}&page_size={}",
            url_path, page_no, MAX_PAGE_SIZE
        )
    };

    if let Some(notify) = notify {
        notify(&url);
    }
    let (headers, body) = match get(&url, REQUEST_TIMEOUT).await {
        Ok(response) => response,
        Err(e) if e.to_string().contains("page out of range") => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    if page_total.is_none() {
        let page_total_str = headers
            .get("X-Page-Total")
            .expect("Missing X-Page-Total header")
            .to_str()
            .expect("X-Page-Total header contains invalid string");
        *page_total =
            Some(page_total_str.parse().unwrap_or_else(|_| {
                panic!("X-Page-Total is an invalid integer: {}", page_total_str)
            }));
    }

    parse(&url, &body)
}

pub async fn request_item_ids<T>(
    url_path: &str,
    item_ids: &[u32],
    cache_dir: Option<&PathBuf>,
    notify: Option<&dyn Fn(&str)>,
) -> Result<Vec<T>, Box<dyn std::error::Error>>
where
    T: serde::Serialize,
    T: serde::de::DeserializeOwned,
{
    let batches = item_ids.chunks(MAX_ITEM_ID_LENGTH as usize);
    // cached fetches notify their own progress
    let progress = notify.filter(|_| cache_dir.is_none());
    if progress.is_some() {
        print!("Batches ({}):", batches.len());
    }
    let done = Cell::new(0);
    let requests = batches.map(|batch| {
        let item_ids_str: Vec<String> = batch.iter().map(|id| id.to_string()).collect();

        let url = format!(
            "https://api.guildwars2.com/v2/{}?ids={}",
            url_path,
            item_ids_str.join(",")
        );
        let done = &done;
        async move {
            if let Some(cache_dir) = cache_dir {
                return cached_fetch::<Vec<T>>(&url, cache_dir, notify).await;
            }
            let result = fetch::<Vec<T>>(&url, None, REQUEST_TIMEOUT).await;
            if progress.is_some() {
                done.set(done.get() + 1);
                print!(" {}", done.get());
                io::stdout()
                    .flush()
                    .unwrap_or_else(|e| println!("Flush failed: {}", &e));
            }
            result
        }
    });
    let request_results = stream::iter(requests)
        .buffered(PARALLEL_REQUESTS)
        .collect::<Vec<Result<Vec<T>, Box<dyn std::error::Error>>>>()
        .await;
    if progress.is_some() {
        println!();
    }

    let mut result = vec![];
    for batch in request_results.into_iter() {
        let mut new_items = batch?;
        result.append(&mut new_items);
    }

    Ok(result)
}

pub async fn fetch_account_recipes(
    key: &str,
    cache_dir: &Path,
    notify: Option<&dyn Fn(&str)>,
) -> Result<HashSet<u32>, Box<dyn std::error::Error>> {
    let url = format!(
        "https://api.guildwars2.com/v2/account/recipes?access_token={}",
        key
    );
    cached_fetch(&url, cache_dir, notify).await
}

/// Mask the API key, which is always the last query parameter
fn redact(url: &str) -> String {
    match url.split_once("access_token=") {
        Some((base, _)) => format!("{}access_token=<api-key>", base),
        None => url.to_string(),
    }
}

async fn cached_fetch<T>(
    url: &str,
    cache_dir: &Path,
    notify: Option<&dyn Fn(&str)>,
) -> Result<T, Box<dyn std::error::Error>>
where
    T: serde::Serialize,
    T: serde::de::DeserializeOwned,
{
    let cache_path = url_to_cache_path(url, cache_dir);
    if let Ok(file) = File::open(&cache_path) {
        if notify.is_some() {
            let age = file.metadata()?.created()?.elapsed().unwrap_or_default();
            println!("Using cached {} from {}s ago", redact(url), age.as_secs());
        }
        let stream = DeflateDecoder::new(file);
        let v = deserialize_from(stream)?;
        return Ok(v);
    }

    let v = fetch(url, notify, REQUEST_TIMEOUT).await?;

    // save cache file
    let file = File::create(cache_path)?;
    let stream = BufWriter::new(DeflateEncoder::new(file, Compression::default()));
    serialize_into(stream, &v)?;

    Ok(v)
}

pub async fn fetch<T>(
    url: &str,
    notify: Option<&dyn Fn(&str)>,
    timeout: Duration,
) -> Result<T, Box<dyn std::error::Error>>
where
    T: serde::de::DeserializeOwned,
{
    if let Some(notify) = notify {
        notify(&redact(url));
    }

    let (_, body) = get(url, timeout).await?;
    parse(url, &body)
}

/// GET a url, retrying failures which may be temporary. Errors mask the API key.
async fn get(
    url: &str,
    timeout: Duration,
) -> Result<(HeaderMap, Vec<u8>), Box<dyn std::error::Error>> {
    let mut attempt = 1;
    loop {
        let failure = match CLIENT.get(url).timeout(timeout).send().await {
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                match response.bytes().await {
                    Ok(body) if status.is_success() && !body.is_empty() => {
                        return Ok((headers, body.to_vec()));
                    }
                    Ok(_) if status.is_success() => "empty response".to_string(),
                    Ok(body) => {
                        let text = error_text(status, &body);
                        if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
                            return Err(format!("{}: {}", redact(url), text).into());
                        }
                        text
                    }
                    Err(e) => e.without_url().to_string(),
                }
            }
            Err(e) => e.without_url().to_string(),
        };

        if attempt >= MAX_ATTEMPTS {
            return Err(format!("{}: {}", redact(url), failure).into());
        }
        eprintln!(
            "{}: {}; retrying ({}/{})",
            redact(url),
            failure,
            attempt,
            MAX_ATTEMPTS - 1
        );
        tokio::time::sleep(Duration::from_millis(
            RandomState::new().build_hasher().finish() % (1000u64 << attempt),
        ))
        .await;
        attempt += 1;
    }
}

fn parse<T>(url: &str, body: &[u8]) -> Result<T, Box<dyn std::error::Error>>
where
    T: serde::de::DeserializeOwned,
{
    let de = &mut serde_json::Deserializer::from_slice(body);
    serde_path_to_error::deserialize(de).map_err(|e| format!("{}: {}", redact(url), e).into())
}

/// The API's message for a failed request, if it sent one
fn error_text(status: StatusCode, body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|error| error.get("text")?.as_str().map(str::to_string))
        .unwrap_or_else(|| status.to_string())
}

fn url_to_cache_path(url: &str, cache_dir: &Path) -> PathBuf {
    let mut hash = DefaultHasher::new();
    url.hash(&mut hash);
    let hash = hash.finish();

    let mut path = cache_dir.to_owned();
    path.push(format!("{}{}", config::CACHE_PREFIX, hash));
    path
}
