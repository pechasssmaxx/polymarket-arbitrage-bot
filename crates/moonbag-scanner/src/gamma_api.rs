//! Gamma API client for dynamic market discovery
//!
//! Fetches neg_risk markets from Polymarket's Gamma API and converts them
//! to StoredMarket format for the scanner.
//!
//! ## Persistence Integration
//!
//! When configured with an `AsyncStateStore`, the client will:
//! - Load convert index cache from disk at startup
//! - Persist new indices when fetched from /neg-risk
//! - Save LKG market snapshot after each successful fetch

use crate::store::AsyncStateStore;
use moonbag_core::models::{StoredMarket, StoredOutcome};
use moonbag_core::{MoonbagError, Result};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Deserialize a JSON-encoded array string like "[\"a\", \"b\"]" -> Vec<String>
fn deserialize_json_string_array<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt {
        Some(s) if s.starts_with('[') => serde_json::from_str(&s)
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

/// Default Gamma API base URL
pub const GAMMA_API_URL: &str = "https://gamma-api.polymarket.com";

/// Default CLOB API URL (for /neg-risk index lookups)
pub const CLOB_API_URL: &str = "https://clob.polymarket.com";

/// Gamma API event response
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GammaEvent {
    pub id: String,
    pub title: String,
    pub ticker: Option<String>,
    pub slug: Option<String>,
    pub neg_risk: Option<bool>,
    #[serde(alias = "negRiskMarketID")]
    pub neg_risk_market_id: Option<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
    pub liquidity: Option<f64>,
    pub volume: Option<f64>,
    pub markets: Option<Vec<GammaMarket>>,
}

/// Gamma API market (outcome) within an event
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GammaMarket {
    pub id: Option<String>,
    pub question: Option<String>,
    pub condition_id: Option<String>,
    /// clobTokenIds comes as a JSON-encoded string: "[\"token1\", \"token2\"]"
    #[serde(default, deserialize_with = "deserialize_json_string_array")]
    pub clob_token_ids: Option<Vec<String>>,
    /// outcomes comes as a JSON-encoded string: "[\"Yes\", \"No\"]"
    #[serde(default, deserialize_with = "deserialize_json_string_array")]
    pub outcomes: Option<Vec<String>>,
    /// groupItemThreshold is the convert index (comes as string "0", "1", etc.)
    pub group_item_threshold: Option<String>,
}

/// CLOB /neg-risk response for getting convert index
#[derive(Debug, Deserialize)]
pub struct NegRiskResponse {
    pub index: Option<u8>,
}

/// Cache for convert indices (token_id -> index)
#[derive(Debug, Default)]
pub struct ConvertIndexCache {
    cache: HashMap<String, u8>,
}

impl ConvertIndexCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create from a HashMap (e.g., loaded from store)
    pub fn from_map(map: HashMap<String, u8>) -> Self {
        Self { cache: map }
    }

    pub fn get(&self, token_id: &str) -> Option<u8> {
        self.cache.get(token_id).copied()
    }

    pub fn insert(&mut self, token_id: String, index: u8) {
        self.cache.insert(token_id, index);
    }

    pub fn contains(&self, token_id: &str) -> bool {
        self.cache.contains_key(token_id)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Get all indices as a HashMap (for persisting to store)
    pub fn to_map(&self) -> HashMap<String, u8> {
        self.cache.clone()
    }

    /// Merge another cache into this one
    pub fn merge(&mut self, other: HashMap<String, u8>) {
        self.cache.extend(other);
    }
}

/// Gamma API client configuration
#[derive(Debug, Clone)]
pub struct GammaApiConfig {
    pub base_url: String,
    pub clob_url: String,
    pub timeout: Duration,
    pub page_limit: usize,
    pub index_fetch_delay: Duration,
    /// Delay between pagination requests to avoid rate limiting
    pub page_fetch_delay: Duration,
    /// Maximum offset to fetch (prevent infinite loop)
    pub max_pagination_offset: usize,
    /// Number of retries for empty/failed pages before giving up
    pub page_retry_count: usize,
    /// Delay before retrying a failed page
    pub page_retry_delay: Duration,
}

impl Default for GammaApiConfig {
    fn default() -> Self {
        Self {
            base_url: GAMMA_API_URL.to_string(),
            clob_url: CLOB_API_URL.to_string(),
            timeout: Duration::from_secs(30),
            page_limit: 500,
            index_fetch_delay: Duration::from_millis(100),
            // Rate limiting: 200ms between pages = max 5 req/sec
            page_fetch_delay: Duration::from_millis(200),
            // Fetch up to offset 10000 (covers 10000+ events)
            max_pagination_offset: 10000,
            // Retry empty pages 3 times (might be rate limiting)
            page_retry_count: 3,
            page_retry_delay: Duration::from_millis(500),
        }
    }
}

/// Gamma API client for fetching neg_risk markets
pub struct GammaApiClient {
    client: reqwest::Client,
    config: GammaApiConfig,
    /// Last Known Good snapshot (used when API fails)
    lkg_snapshot: RwLock<Vec<StoredMarket>>,
    /// Convert index cache (persists between refresh cycles)
    index_cache: RwLock<ConvertIndexCache>,
    /// Optional persistent store for fast restart
    store: Option<Arc<AsyncStateStore>>,
}

impl GammaApiClient {
    /// Create a new Gamma API client
    pub fn new(config: GammaApiConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            config,
            lkg_snapshot: RwLock::new(Vec::new()),
            index_cache: RwLock::new(ConvertIndexCache::new()),
            store: None,
        }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(GammaApiConfig::default())
    }

    /// Create with persistent store
    ///
    /// Loads LKG snapshot and convert index cache from store.
    pub fn with_store(config: GammaApiConfig, store: Arc<AsyncStateStore>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("Failed to create HTTP client");

        // Load LKG from store
        let lkg_markets = store.get_lkg_markets();
        info!(count = lkg_markets.len(), "Loaded LKG markets from store");

        // Load convert index cache from store
        let indices = store.get_all_convert_indices();
        info!(count = indices.len(), "Loaded convert indices from store");

        Self {
            client,
            config,
            lkg_snapshot: RwLock::new(lkg_markets),
            index_cache: RwLock::new(ConvertIndexCache::from_map(indices)),
            store: Some(store),
        }
    }

    /// Set the store after creation (for deferred initialization)
    pub fn set_store(&mut self, store: Arc<AsyncStateStore>) {
        // Load existing data from store
        let lkg_markets = store.get_lkg_markets();
        if !lkg_markets.is_empty() {
            let mut lkg = self.lkg_snapshot.write().unwrap();
            *lkg = lkg_markets;
            info!(count = lkg.len(), "Loaded LKG markets from store");
        }

        let indices = store.get_all_convert_indices();
        if !indices.is_empty() {
            let mut cache = self.index_cache.write().unwrap();
            cache.merge(indices.clone());
            info!(count = indices.len(), "Merged convert indices from store");
        }

        self.store = Some(store);
    }

    /// Get the Last Known Good snapshot
    pub fn get_lkg_snapshot(&self) -> Vec<StoredMarket> {
        self.lkg_snapshot.read().unwrap().clone()
    }

    /// Get index cache stats
    pub fn index_cache_size(&self) -> usize {
        self.index_cache.read().unwrap().len()
    }

    /// Cache a convert index (and persist to store if available)
    fn cache_convert_index(&self, token_id: &str, index: u8) {
        // Update in-memory cache
        {
            let mut cache = self.index_cache.write().unwrap();
            cache.insert(token_id.to_string(), index);
        }

        // Persist to store (async, fire-and-forget)
        if let Some(store) = &self.store {
            store.set_convert_index(token_id.to_string(), index);
        }
    }

    /// Persist current state to store (if available)
    pub fn persist_state(&self) {
        let Some(store) = &self.store else { return };

        // Save LKG markets
        let lkg = self.lkg_snapshot.read().unwrap().clone();
        if !lkg.is_empty() {
            store.set_lkg_markets(lkg);
        }

        // Save convert index cache
        let indices = self.index_cache.read().unwrap().to_map();
        if !indices.is_empty() {
            store.set_convert_indices(indices);
        }
    }

    /// Fetch all active neg_risk events with pagination
    ///
    /// Returns StoredMarket list. On 5xx errors, returns LKG snapshot.
    /// Uses rate limiting and retry logic to handle API throttling.
    pub async fn fetch_neg_risk_markets(&self) -> Result<Vec<StoredMarket>> {
        let mut all_events = Vec::new();
        let mut offset = 0;
        let mut consecutive_empty_pages = 0;
        let mut total_pages_fetched = 0;
        let start_time = std::time::Instant::now();

        info!(
            "Starting Gamma API pagination (max_offset={}, page_limit={}, delay={}ms)",
            self.config.max_pagination_offset,
            self.config.page_limit,
            self.config.page_fetch_delay.as_millis()
        );

        loop {
            // Check max offset limit
            if offset >= self.config.max_pagination_offset {
                info!(
                    "Reached max pagination offset {} at {} events",
                    offset,
                    all_events.len()
                );
                break;
            }

            let url = format!(
                "{}/events?neg_risk=true&active=true&closed=false&limit={}&offset={}",
                self.config.base_url, self.config.page_limit, offset
            );

            debug!("Fetching Gamma API: {}", url);

            // Fetch with retry logic
            let mut page_events: Option<Vec<GammaEvent>> = None;
            for retry in 0..=self.config.page_retry_count {
                if retry > 0 {
                    debug!(
                        "Retry {}/{} for offset {} after empty/failed response",
                        retry, self.config.page_retry_count, offset
                    );
                    tokio::time::sleep(self.config.page_retry_delay).await;
                }

                let response = match self.client.get(&url).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        if retry == self.config.page_retry_count {
                            error!("Gamma API request failed after retries: {}", e);
                            // Return LKG on persistent network error
                            return Ok(self.get_lkg_snapshot());
                        }
                        continue;
                    }
                };

                let status = response.status();
                if status.is_server_error() {
                    if retry == self.config.page_retry_count {
                        warn!(
                            "Gamma API returned {} after retries, using LKG snapshot",
                            status
                        );
                        return Ok(self.get_lkg_snapshot());
                    }
                    continue;
                }

                if !status.is_success() {
                    if retry == self.config.page_retry_count {
                        return Err(MoonbagError::ClobApi {
                            message: format!("Gamma API error: {}", status),
                            status: Some(status.as_u16()),
                        });
                    }
                    continue;
                }

                match response.json::<Vec<GammaEvent>>().await {
                    Ok(events) => {
                        if !events.is_empty() || retry == self.config.page_retry_count {
                            page_events = Some(events);
                            break;
                        }
                        // Empty response, might be rate limiting - retry
                        debug!("Got empty response at offset {}, retrying...", offset);
                    }
                    Err(e) => {
                        if retry == self.config.page_retry_count {
                            warn!("Failed to parse Gamma response after retries: {}", e);
                            // Continue with what we have instead of failing
                            break;
                        }
                    }
                }
            }

            let events = page_events.unwrap_or_default();
            let page_count = events.len();
            total_pages_fetched += 1;

            // Filter for neg_risk only (API filter doesn't work reliably)
            let neg_risk_events: Vec<_> = events
                .into_iter()
                .filter(|e| e.neg_risk.unwrap_or(false) && e.neg_risk_market_id.is_some())
                .collect();

            all_events.extend(neg_risk_events);

            // Log progress every 10 pages
            if total_pages_fetched % 10 == 0 {
                info!(
                    "Pagination progress: offset={}, pages={}, neg_risk_events={}, elapsed={:.1}s",
                    offset,
                    total_pages_fetched,
                    all_events.len(),
                    start_time.elapsed().as_secs_f64()
                );
            }

            // Check for empty page
            if page_count == 0 {
                consecutive_empty_pages += 1;
                debug!(
                    "Empty page at offset {} (consecutive: {})",
                    offset, consecutive_empty_pages
                );
                // Allow a few consecutive empty pages (API might be flaky)
                if consecutive_empty_pages >= 3 {
                    info!(
                        "Stopping pagination after {} consecutive empty pages at offset {}",
                        consecutive_empty_pages, offset
                    );
                    break;
                }
                // Still increment offset to try next page
                offset += self.config.page_limit;
            } else {
                consecutive_empty_pages = 0;
                offset += page_count;
            }

            // Rate limiting: delay between requests
            tokio::time::sleep(self.config.page_fetch_delay).await;
        }

        info!(
            "Fetched {} neg_risk events from Gamma API (pages={}, offset={}, elapsed={:.1}s)",
            all_events.len(),
            total_pages_fetched,
            offset,
            start_time.elapsed().as_secs_f64()
        );

        // Convert to StoredMarket
        let markets = self.convert_events_to_markets(all_events).await?;

        // Update LKG snapshot
        {
            let mut lkg = self.lkg_snapshot.write().unwrap();
            *lkg = markets.clone();
        }

        // Persist to store (async, fire-and-forget)
        if let Some(store) = &self.store {
            store.set_lkg_markets(markets.clone());
        }

        Ok(markets)
    }

    /// Convert Gamma events to StoredMarket format
    async fn convert_events_to_markets(
        &self,
        events: Vec<GammaEvent>,
    ) -> Result<Vec<StoredMarket>> {
        // Some NegRisk markets don't include `groupItemThreshold` in Gamma responses.
        // Those are still tradable/convertible, but we must fetch the onchain convert index
        // from CLOB `/neg-risk` (cached + persisted).
        let mut tokens_needing_index: Vec<String> = Vec::new();
        for event in &events {
            let Some(gamma_markets) = event.markets.as_ref() else {
                continue;
            };

            for gm in gamma_markets {
                let Some(question) = gm.question.as_ref() else {
                    continue;
                };
                let Some(clob_token_ids) = gm.clob_token_ids.as_ref() else {
                    continue;
                };
                if clob_token_ids.len() < 2 {
                    continue;
                }

                // Determine YES token id (see main conversion pass for details)
                let yes_token_id = if let Some(outcomes) = gm.outcomes.as_ref() {
                    if let (Some(yi), Some(ni)) = (
                        outcomes.iter().position(|o| o.to_lowercase() == "yes"),
                        outcomes.iter().position(|o| o.to_lowercase() == "no"),
                    ) {
                        if yi < clob_token_ids.len() && ni < clob_token_ids.len() {
                            clob_token_ids[yi].clone()
                        } else {
                            clob_token_ids[0].clone()
                        }
                    } else {
                        clob_token_ids[0].clone()
                    }
                } else {
                    clob_token_ids[0].clone()
                };

                let threshold_ok = gm
                    .group_item_threshold
                    .as_ref()
                    .and_then(|t| t.parse::<u8>().ok())
                    .is_some();
                if threshold_ok {
                    continue;
                }

                let in_cache = {
                    let cache = self.index_cache.read().unwrap();
                    cache.contains(&yes_token_id)
                };
                if !in_cache {
                    debug!(
                        question = %question,
                        yes_token_id = %yes_token_id,
                        "Missing groupItemThreshold - will fetch convert index from /neg-risk"
                    );
                    tokens_needing_index.push(yes_token_id);
                }
            }
        }

        if !tokens_needing_index.is_empty() {
            tokens_needing_index.sort();
            tokens_needing_index.dedup();
            let _ = self
                .fetch_convert_indices_batch(&tokens_needing_index)
                .await;
        }

        let mut markets = Vec::with_capacity(events.len());
        let mut missing_threshold = 0u64;
        let mut recovered_from_cache = 0u64;

        for event in events {
            let Some(neg_risk_market_id) = event.neg_risk_market_id else {
                continue;
            };

            let Some(gamma_markets) = event.markets else {
                continue;
            };

            // Convert each market (outcome) to StoredOutcome
            let mut outcomes = Vec::with_capacity(gamma_markets.len());

            for (array_idx, gm) in gamma_markets.into_iter().enumerate() {
                let Some(question) = gm.question else {
                    continue;
                };

                let Some(condition_id) = gm.condition_id else {
                    continue;
                };

                let Some(clob_token_ids) = gm.clob_token_ids else {
                    continue;
                };

                // clobTokenIds[0] = YES, clobTokenIds[1] = NO
                // But we should verify by checking outcomes array
                let (yes_token_id, no_token_id) = if clob_token_ids.len() >= 2 {
                    let outcomes_arr = gm.outcomes.as_ref();

                    // Check if outcomes array tells us the order
                    if let Some(outcomes) = outcomes_arr {
                        if outcomes.len() >= 2 {
                            // Find YES and NO positions
                            let yes_idx = outcomes.iter().position(|o| o.to_lowercase() == "yes");
                            let no_idx = outcomes.iter().position(|o| o.to_lowercase() == "no");

                            match (yes_idx, no_idx) {
                                (Some(yi), Some(ni))
                                    if yi < clob_token_ids.len() && ni < clob_token_ids.len() =>
                                {
                                    (clob_token_ids[yi].clone(), clob_token_ids[ni].clone())
                                }
                                _ => {
                                    // Default: first = YES, second = NO
                                    (clob_token_ids[0].clone(), clob_token_ids[1].clone())
                                }
                            }
                        } else {
                            (clob_token_ids[0].clone(), clob_token_ids[1].clone())
                        }
                    } else {
                        (clob_token_ids[0].clone(), clob_token_ids[1].clone())
                    }
                } else {
                    continue; // Skip if we don't have both tokens
                };

                // Get convert index from groupItemThreshold field
                // This is the onchain CONVERT index (NOT alphabetical sorting)
                let threshold_idx = gm
                    .group_item_threshold
                    .as_ref()
                    .and_then(|t| t.parse::<u8>().ok());
                let alphabetical_index = match threshold_idx {
                    Some(idx) => {
                        // Cache the index for future use
                        self.cache_convert_index(&yes_token_id, idx);
                        idx
                    }
                    None => {
                        missing_threshold += 1;
                        // Try cache first
                        let cached_idx = {
                            let cache = self.index_cache.read().unwrap();
                            cache.get(&yes_token_id)
                        };
                        if let Some(idx) = cached_idx {
                            recovered_from_cache += 1;
                            idx
                        } else {
                            // Fallback: use array index (Polymarket typically uses creation order)
                            let idx = array_idx as u8;
                            debug!(
                                question = %question,
                                yes_token_id = %yes_token_id,
                                fallback_idx = idx,
                                "Using array index as fallback for missing groupItemThreshold"
                            );
                            // Cache for future use
                            self.cache_convert_index(&yes_token_id, idx);
                            idx
                        }
                    }
                };

                outcomes.push(StoredOutcome {
                    question,
                    condition_id,
                    yes_token_id,
                    no_token_id,
                    alphabetical_index,
                });
            }

            if outcomes.is_empty() {
                continue;
            }

            // All outcomes are convertible by default
            let convertible_indices: Vec<u8> =
                outcomes.iter().map(|o| o.alphabetical_index).collect();

            let slug = event
                .ticker
                .or(event.slug)
                .unwrap_or_else(|| event.id.clone());

            markets.push(StoredMarket {
                neg_risk_market_id,
                title: event.title,
                slug,
                outcome_count: outcomes.len(),
                outcomes,
                convertible_indices,
                collected_at: chrono::Utc::now().to_rfc3339(),
                event_id: Some(event.id),
                active: event.active,
                closed: event.closed,
                liquidity: event.liquidity,
                volume: event.volume,
                volume24hr: None,
                open_interest: None,
                updated_at: None,
            });
        }

        if missing_threshold > 0 {
            info!(
                missing_threshold,
                recovered_from_cache,
                used_array_fallback = missing_threshold - recovered_from_cache,
                "Gamma: resolved missing groupItemThreshold outcomes"
            );
        }

        Ok(markets)
    }

    /// Get convert index from cache or fetch from CLOB /neg-risk
    async fn get_or_fetch_convert_index(&self, token_id: &str) -> Option<u8> {
        // Check cache first
        {
            let cache = self.index_cache.read().unwrap();
            if let Some(index) = cache.get(token_id) {
                return Some(index);
            }
        }

        // Fetch from CLOB
        let index = self.fetch_convert_index(token_id).await?;

        // Update cache
        {
            let mut cache = self.index_cache.write().unwrap();
            cache.insert(token_id.to_string(), index);
        }

        // Persist to store (async, fire-and-forget)
        if let Some(store) = &self.store {
            store.set_convert_index(token_id.to_string(), index);
        }

        Some(index)
    }

    /// Fetch convert index from CLOB /neg-risk endpoint
    async fn fetch_convert_index(&self, token_id: &str) -> Option<u8> {
        let url = format!("{}/neg-risk", self.config.clob_url);

        let response = self
            .client
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await
            .ok()?;

        if !response.status().is_success() {
            debug!(
                "Failed to fetch /neg-risk for {}: {}",
                token_id,
                response.status()
            );
            return None;
        }

        let data: NegRiskResponse = response.json().await.ok()?;
        data.index
    }

    /// Fetch convert indices for new tokens only (batch)
    ///
    /// This method batches fetches and persists all new indices at the end
    /// (more efficient than per-token persistence in get_or_fetch_convert_index).
    pub async fn fetch_convert_indices_batch(&self, token_ids: &[String]) -> usize {
        // Filter to only new tokens
        let new_tokens: Vec<_> = {
            let cache = self.index_cache.read().unwrap();
            token_ids
                .iter()
                .filter(|t| !cache.contains(t))
                .cloned()
                .collect()
        };

        if new_tokens.is_empty() {
            return 0;
        }

        info!("Fetching {} new convert indices", new_tokens.len());

        // Collect all fetched indices for batch persistence
        let mut new_indices = HashMap::new();

        // Bounded concurrency with rate limiting
        for chunk in new_tokens.chunks(10) {
            for token_id in chunk {
                if let Some(index) = self.fetch_convert_index(token_id).await {
                    new_indices.insert(token_id.clone(), index);
                }
            }
            tokio::time::sleep(self.config.index_fetch_delay).await;
        }

        let fetched = new_indices.len();

        // Update RAM cache
        {
            let mut cache = self.index_cache.write().unwrap();
            for (token_id, index) in &new_indices {
                cache.insert(token_id.clone(), *index);
            }
        }

        // Persist to store in one batch (async, fire-and-forget)
        if let Some(store) = &self.store {
            store.set_convert_indices(new_indices);
        }

        info!("Fetched {} convert indices", fetched);
        fetched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_index_cache() {
        let mut cache = ConvertIndexCache::new();
        assert!(cache.is_empty());

        cache.insert("token1".to_string(), 0);
        cache.insert("token2".to_string(), 1);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("token1"), Some(0));
        assert_eq!(cache.get("token2"), Some(1));
        assert_eq!(cache.get("token3"), None);
        assert!(cache.contains("token1"));
        assert!(!cache.contains("token3"));
    }

    #[tokio::test]
    async fn test_missing_group_item_threshold_uses_cached_neg_risk_index() {
        let client = GammaApiClient::with_defaults();

        // Pre-seed the cache so conversion doesn't attempt any network fetch.
        {
            let mut cache = client.index_cache.write().unwrap();
            cache.insert("yes_token".to_string(), 7);
        }

        let event = GammaEvent {
            id: "e1".to_string(),
            title: "Test Event".to_string(),
            ticker: None,
            slug: None,
            neg_risk: Some(true),
            neg_risk_market_id: Some("0xdeadbeef".to_string()),
            active: Some(true),
            closed: Some(false),
            liquidity: None,
            volume: None,
            markets: Some(vec![GammaMarket {
                id: Some("m1".to_string()),
                question: Some("Test Question".to_string()),
                condition_id: Some("cond".to_string()),
                clob_token_ids: Some(vec!["yes_token".to_string(), "no_token".to_string()]),
                outcomes: Some(vec!["Yes".to_string(), "No".to_string()]),
                group_item_threshold: None,
            }]),
        };

        let markets = client.convert_events_to_markets(vec![event]).await.unwrap();
        assert_eq!(markets.len(), 1);
        assert_eq!(markets[0].outcomes.len(), 1);
        assert_eq!(markets[0].outcomes[0].alphabetical_index, 7);
        assert_eq!(markets[0].convertible_indices, vec![7]);
    }
}
