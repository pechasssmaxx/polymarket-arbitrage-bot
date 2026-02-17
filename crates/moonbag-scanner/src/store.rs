//! Persistent storage for scanner state using redb.
//!
//! Stores:
//! - Convert index cache (token_id → index) - critical for CONVERT
//! - LKG (Last Known Good) market snapshot - for fast restart
//! - Opportunity log (append-only) - for audit/debugging
//!
//! ## Design
//!
//! All writes are async (fire-and-forget via mpsc channel) to avoid blocking
//! the hot path. Reads are synchronous but fast (mmap).

use moonbag_core::models::{MoonbagOpportunity, StoredMarket};
use moonbag_core::{MoonbagError, Result};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// Table definitions
const CONVERT_INDEX_TABLE: TableDefinition<&str, u8> = TableDefinition::new("convert_index");
const LKG_MARKETS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("lkg_markets");
const OPPORTUNITY_LOG_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("opportunity_log");

/// Key for LKG markets snapshot
const LKG_KEY: &str = "snapshot";

/// Persistent state store trait
pub trait StateStore: Send + Sync {
    /// Get convert index for a token
    fn get_convert_index(&self, token_id: &str) -> Option<u8>;

    /// Set convert index for a token
    fn set_convert_index(&self, token_id: &str, index: u8);

    /// Get all convert indices
    fn get_all_convert_indices(&self) -> HashMap<String, u8>;

    /// Bulk set convert indices
    fn set_convert_indices(&self, indices: &HashMap<String, u8>);

    /// Get LKG markets snapshot
    fn get_lkg_markets(&self) -> Vec<StoredMarket>;

    /// Set LKG markets snapshot
    fn set_lkg_markets(&self, markets: &[StoredMarket]);

    /// Log an opportunity (append-only)
    fn log_opportunity(&self, opp: &MoonbagOpportunity);
}

/// Redb-backed persistent store
pub struct RedbStore {
    db: Database,
}

impl RedbStore {
    /// Open or create a redb database at the given path
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MoonbagError::Other(format!("Failed to create data directory: {}", e))
            })?;
        }

        let db = Database::create(path).map_err(|e| {
            MoonbagError::Other(format!("Failed to open redb at {:?}: {}", path, e))
        })?;

        // Create tables if they don't exist
        let write_txn = db
            .begin_write()
            .map_err(|e| MoonbagError::Other(format!("Failed to begin write txn: {}", e)))?;
        {
            write_txn.open_table(CONVERT_INDEX_TABLE).map_err(|e| {
                MoonbagError::Other(format!("Failed to create convert_index table: {}", e))
            })?;
            write_txn.open_table(LKG_MARKETS_TABLE).map_err(|e| {
                MoonbagError::Other(format!("Failed to create lkg_markets table: {}", e))
            })?;
            write_txn.open_table(OPPORTUNITY_LOG_TABLE).map_err(|e| {
                MoonbagError::Other(format!("Failed to create opportunity_log table: {}", e))
            })?;
        }
        write_txn
            .commit()
            .map_err(|e| MoonbagError::Other(format!("Failed to commit table creation: {}", e)))?;

        info!(path = ?path, "Opened redb store");
        Ok(Self { db })
    }

    /// Get the number of convert indices stored
    pub fn convert_index_count(&self) -> usize {
        let Ok(read_txn) = self.db.begin_read() else {
            return 0;
        };
        let Ok(table) = read_txn.open_table(CONVERT_INDEX_TABLE) else {
            return 0;
        };
        table.len().unwrap_or(0) as usize
    }

    /// Get the number of opportunities logged
    pub fn opportunity_count(&self) -> usize {
        let Ok(read_txn) = self.db.begin_read() else {
            return 0;
        };
        let Ok(table) = read_txn.open_table(OPPORTUNITY_LOG_TABLE) else {
            return 0;
        };
        table.len().unwrap_or(0) as usize
    }
}

impl StateStore for RedbStore {
    fn get_convert_index(&self, token_id: &str) -> Option<u8> {
        let read_txn = self.db.begin_read().ok()?;
        let table = read_txn.open_table(CONVERT_INDEX_TABLE).ok()?;
        table.get(token_id).ok()?.map(|v| v.value())
    }

    fn set_convert_index(&self, token_id: &str, index: u8) {
        let Ok(write_txn) = self.db.begin_write() else {
            warn!("Failed to begin write txn for convert_index");
            return;
        };
        let insert_result = {
            let Ok(mut table) = write_txn.open_table(CONVERT_INDEX_TABLE) else {
                warn!("Failed to open convert_index table");
                return;
            };
            table.insert(token_id, index).map(|_| ())
        };
        if let Err(e) = insert_result {
            warn!(token_id, error = %e, "Failed to insert convert_index");
            return;
        }
        if let Err(e) = write_txn.commit() {
            warn!(error = %e, "Failed to commit convert_index");
        }
    }

    fn get_all_convert_indices(&self) -> HashMap<String, u8> {
        let mut result = HashMap::new();

        let Ok(read_txn) = self.db.begin_read() else {
            return result;
        };
        let Ok(table) = read_txn.open_table(CONVERT_INDEX_TABLE) else {
            return result;
        };

        let Ok(iter) = table.iter() else {
            return result;
        };

        for entry in iter.flatten() {
            let key = entry.0.value().to_string();
            let value = entry.1.value();
            result.insert(key, value);
        }

        debug!(count = result.len(), "Loaded convert indices from store");
        result
    }

    fn set_convert_indices(&self, indices: &HashMap<String, u8>) {
        if indices.is_empty() {
            return;
        }

        let Ok(write_txn) = self.db.begin_write() else {
            warn!("Failed to begin write txn for bulk convert_index");
            return;
        };
        {
            let Ok(mut table) = write_txn.open_table(CONVERT_INDEX_TABLE) else {
                warn!("Failed to open convert_index table for bulk write");
                return;
            };
            for (token_id, index) in indices {
                if let Err(e) = table.insert(token_id.as_str(), *index) {
                    warn!(token_id, error = %e, "Failed to insert convert_index in bulk");
                }
            }
        }
        if let Err(e) = write_txn.commit() {
            warn!(error = %e, "Failed to commit bulk convert_index");
        } else {
            debug!(count = indices.len(), "Saved convert indices to store");
        }
    }

    fn get_lkg_markets(&self) -> Vec<StoredMarket> {
        let Ok(read_txn) = self.db.begin_read() else {
            return Vec::new();
        };
        let Ok(table) = read_txn.open_table(LKG_MARKETS_TABLE) else {
            return Vec::new();
        };

        let Some(entry) = table.get(LKG_KEY).ok().flatten() else {
            return Vec::new();
        };

        let bytes = entry.value();
        match serde_json::from_slice::<Vec<StoredMarket>>(bytes) {
            Ok(markets) => {
                info!(count = markets.len(), "Loaded LKG markets from store");
                markets
            }
            Err(e) => {
                error!(error = %e, "Failed to deserialize LKG markets");
                Vec::new()
            }
        }
    }

    fn set_lkg_markets(&self, markets: &[StoredMarket]) {
        let bytes = match serde_json::to_vec(markets) {
            Ok(b) => b,
            Err(e) => {
                error!(error = %e, "Failed to serialize LKG markets");
                return;
            }
        };

        let Ok(write_txn) = self.db.begin_write() else {
            warn!("Failed to begin write txn for LKG markets");
            return;
        };
        let insert_result = {
            let Ok(mut table) = write_txn.open_table(LKG_MARKETS_TABLE) else {
                warn!("Failed to open LKG markets table");
                return;
            };
            table.insert(LKG_KEY, bytes.as_slice()).map(|_| ())
        };
        if let Err(e) = insert_result {
            warn!(error = %e, "Failed to insert LKG markets");
            return;
        }
        if let Err(e) = write_txn.commit() {
            warn!(error = %e, "Failed to commit LKG markets");
        } else {
            info!(count = markets.len(), "Saved LKG markets to store");
        }
    }

    fn log_opportunity(&self, opp: &MoonbagOpportunity) {
        let bytes = match serde_json::to_vec(opp) {
            Ok(b) => b,
            Err(e) => {
                error!(error = %e, "Failed to serialize opportunity");
                return;
            }
        };

        // Use timestamp as key (unix millis), handle collisions by incrementing
        let base_key = opp.timestamp.timestamp_millis() as u64;

        let Ok(write_txn) = self.db.begin_write() else {
            warn!("Failed to begin write txn for opportunity log");
            return;
        };

        let insert_result = {
            let Ok(mut table) = write_txn.open_table(OPPORTUNITY_LOG_TABLE) else {
                warn!("Failed to open opportunity log table");
                return;
            };

            // Find a free key slot (handle timestamp collisions)
            let mut key = base_key;
            const MAX_COLLISION_ATTEMPTS: u64 = 100;
            let mut found_slot = false;
            for _ in 0..MAX_COLLISION_ATTEMPTS {
                if table.get(key).ok().flatten().is_none() {
                    found_slot = true;
                    break;
                }
                key += 1;
            }

            if !found_slot {
                warn!(
                    base_key = base_key,
                    "Failed to find free slot after {} attempts", MAX_COLLISION_ATTEMPTS
                );
                return;
            }

            table.insert(key, bytes.as_slice()).map(|_| ())
        };

        if let Err(e) = insert_result {
            warn!(error = %e, "Failed to insert opportunity");
            return;
        }
        if let Err(e) = write_txn.commit() {
            warn!(error = %e, "Failed to commit opportunity");
        } else {
            debug!(
                market = %opp.market_title,
                profit = %opp.guaranteed_profit,
                "Logged opportunity to store"
            );
        }
    }
}

/// Async wrapper for non-blocking writes
///
/// Wraps RedbStore and provides async methods that send writes
/// to a background task via mpsc channel.
///
/// Uses a bounded channel (capacity 1000) with backpressure:
/// - `try_send` for fire-and-forget (drops on full queue)
/// - Dropped writes are counted for observability
pub struct AsyncStateStore {
    store: Arc<RedbStore>,
    write_tx: mpsc::Sender<WriteOp>,
    dropped_writes: std::sync::atomic::AtomicU64,
}

enum WriteOp {
    SetConvertIndex { token_id: String, index: u8 },
    SetConvertIndices(HashMap<String, u8>),
    SetLkgMarkets(Vec<StoredMarket>),
    LogOpportunity(Box<MoonbagOpportunity>),
}

/// Default capacity for the write queue
const WRITE_QUEUE_CAPACITY: usize = 1000;

impl AsyncStateStore {
    /// Create a new async store wrapper with default queue capacity
    pub fn new(store: Arc<RedbStore>) -> Self {
        Self::with_capacity(store, WRITE_QUEUE_CAPACITY)
    }

    /// Create with custom queue capacity
    pub fn with_capacity(store: Arc<RedbStore>, capacity: usize) -> Self {
        let (write_tx, mut write_rx) = mpsc::channel::<WriteOp>(capacity);
        let store_clone = store.clone();

        // Spawn background writer task
        tokio::spawn(async move {
            while let Some(op) = write_rx.recv().await {
                let store = store_clone.clone();

                // Use spawn_blocking for blocking redb I/O to avoid stalling tokio runtime
                let result = tokio::task::spawn_blocking(move || match op {
                    WriteOp::SetConvertIndex { token_id, index } => {
                        store.set_convert_index(&token_id, index);
                    }
                    WriteOp::SetConvertIndices(indices) => {
                        store.set_convert_indices(&indices);
                    }
                    WriteOp::SetLkgMarkets(markets) => {
                        store.set_lkg_markets(&markets);
                    }
                    WriteOp::LogOpportunity(opp) => {
                        store.log_opportunity(&opp);
                    }
                })
                .await;

                if let Err(e) = result {
                    warn!(error = %e, "spawn_blocking for redb write failed");
                }
            }
            debug!("AsyncStateStore writer task exiting");
        });

        Self {
            store,
            write_tx,
            dropped_writes: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Get count of dropped writes (queue was full)
    pub fn dropped_writes(&self) -> u64 {
        self.dropped_writes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Try to send a write op, increment dropped counter on failure
    fn try_send(&self, op: WriteOp) {
        if self.write_tx.try_send(op).is_err() {
            self.dropped_writes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Get convert index (sync read, fast via mmap)
    pub fn get_convert_index(&self, token_id: &str) -> Option<u8> {
        self.store.get_convert_index(token_id)
    }

    /// Set convert index (async, fire-and-forget)
    pub fn set_convert_index(&self, token_id: String, index: u8) {
        self.try_send(WriteOp::SetConvertIndex { token_id, index });
    }

    /// Get all convert indices (sync read)
    pub fn get_all_convert_indices(&self) -> HashMap<String, u8> {
        self.store.get_all_convert_indices()
    }

    /// Set all convert indices (async, fire-and-forget)
    pub fn set_convert_indices(&self, indices: HashMap<String, u8>) {
        self.try_send(WriteOp::SetConvertIndices(indices));
    }

    /// Get LKG markets (sync read)
    pub fn get_lkg_markets(&self) -> Vec<StoredMarket> {
        self.store.get_lkg_markets()
    }

    /// Set LKG markets (async, fire-and-forget)
    pub fn set_lkg_markets(&self, markets: Vec<StoredMarket>) {
        self.try_send(WriteOp::SetLkgMarkets(markets));
    }

    /// Log opportunity (async, fire-and-forget)
    pub fn log_opportunity(&self, opp: MoonbagOpportunity) {
        self.try_send(WriteOp::LogOpportunity(Box::new(opp)));
    }

    /// Get inner store for stats
    pub fn inner(&self) -> &RedbStore {
        &self.store
    }
}

/// Configuration for the store
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Path to redb database file
    pub path: String,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            path: "./data/moonbag.redb".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_convert_index_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let store = RedbStore::open(&path).unwrap();

        // Initially empty
        assert!(store.get_convert_index("token1").is_none());
        assert_eq!(store.convert_index_count(), 0);

        // Set and get
        store.set_convert_index("token1", 5);
        assert_eq!(store.get_convert_index("token1"), Some(5));
        assert_eq!(store.convert_index_count(), 1);

        // Bulk set
        let mut indices = HashMap::new();
        indices.insert("token2".to_string(), 10);
        indices.insert("token3".to_string(), 15);
        store.set_convert_indices(&indices);

        assert_eq!(store.get_convert_index("token2"), Some(10));
        assert_eq!(store.get_convert_index("token3"), Some(15));
        assert_eq!(store.convert_index_count(), 3);

        // Get all
        let all = store.get_all_convert_indices();
        assert_eq!(all.len(), 3);
        assert_eq!(all.get("token1"), Some(&5));
    }

    #[test]
    fn test_lkg_markets_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let store = RedbStore::open(&path).unwrap();

        // Initially empty
        assert!(store.get_lkg_markets().is_empty());

        // Create test markets
        let markets = vec![StoredMarket {
            neg_risk_market_id: "0x123".to_string(),
            title: "Test Market".to_string(),
            slug: "test-market".to_string(),
            outcomes: vec![],
            outcome_count: 2,
            collected_at: "2024-01-01".to_string(),
            convertible_indices: vec![0, 1],
            event_id: None,
            active: Some(true),
            closed: Some(false),
            liquidity: None,
            volume: None,
            volume24hr: None,
            open_interest: None,
            updated_at: None,
        }];

        store.set_lkg_markets(&markets);
        let loaded = store.get_lkg_markets();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].neg_risk_market_id, "0x123");
        assert_eq!(loaded[0].title, "Test Market");
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.redb");

        // First session: write data
        {
            let store = RedbStore::open(&path).unwrap();
            store.set_convert_index("persistent_token", 42);
        }

        // Second session: read data
        {
            let store = RedbStore::open(&path).unwrap();
            assert_eq!(store.get_convert_index("persistent_token"), Some(42));
        }
    }
}
