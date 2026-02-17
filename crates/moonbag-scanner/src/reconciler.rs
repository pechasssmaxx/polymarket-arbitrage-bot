//! Market reconciler with hysteresis for stable market management
//!
//! Handles adding new markets and removing resolved/stale ones with:
//! - Immediate removal for resolved markets (closed=true OR active=false)
//! - Hysteresis (N consecutive misses) before removing missing markets
//! - Debounced WS resubscribe to avoid thrashing

use moonbag_core::models::StoredMarket;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Configuration for the market reconciler
#[derive(Debug, Clone)]
pub struct ReconcilerConfig {
    /// Number of consecutive misses before removing a market (default: 3)
    pub miss_threshold: u8,
    /// Minimum time between WS resubscribes (default: 10s)
    pub resubscribe_debounce: Duration,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            miss_threshold: 3,
            resubscribe_debounce: Duration::from_secs(10),
        }
    }
}

/// Result of market reconciliation
#[derive(Debug, Default)]
pub struct ReconcileResult {
    /// Markets to add (new or re-activated)
    pub added: Vec<StoredMarket>,
    /// Market IDs to remove (after miss threshold)
    pub removed: Vec<String>,
    /// Whether WS should resubscribe (with debounce)
    pub needs_resubscribe: bool,
}

impl ReconcileResult {
    pub fn has_changes(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }
}

/// Market reconciler with hysteresis
///
/// Rules:
/// - resolved = (closed == true) OR (active == false) → remove IMMEDIATELY
/// - new market → add, clear any miss count
/// - existing market → clear miss count
/// - missing market → increment miss count, remove after N consecutive misses
pub struct MarketReconciler {
    config: ReconcilerConfig,
    /// market_id → consecutive miss count
    miss_counts: HashMap<String, u8>,
    /// Last time we triggered a resubscribe
    last_resubscribe: Instant,
}

impl MarketReconciler {
    pub fn new(config: ReconcilerConfig) -> Self {
        Self {
            config,
            miss_counts: HashMap::new(),
            last_resubscribe: Instant::now() - Duration::from_secs(60), // Allow immediate first resubscribe
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(ReconcilerConfig::default())
    }

    /// Reconcile current markets with fresh data from Gamma API
    ///
    /// # Arguments
    /// * `current_ids` - Currently tracked market IDs
    /// * `fresh` - Fresh markets from Gamma API
    ///
    /// # Returns
    /// `ReconcileResult` with markets to add/remove and resubscribe flag
    pub fn reconcile(&mut self, current_ids: &[String], fresh: &[StoredMarket]) -> ReconcileResult {
        let mut result = ReconcileResult::default();
        let current_set: HashSet<&str> = current_ids.iter().map(|s| s.as_str()).collect();
        let mut fresh_ids: HashSet<&str> = HashSet::new();

        // Process fresh markets
        for market in fresh {
            let market_id = &market.neg_risk_market_id;
            fresh_ids.insert(market_id.as_str());

            // RULE: resolved = closed || !active → remove immediately
            let is_resolved = market.closed.unwrap_or(false) || !market.active.unwrap_or(true);

            if is_resolved {
                if current_set.contains(market_id.as_str()) {
                    debug!(market_id = %market_id, "Removing resolved market");
                    result.removed.push(market_id.clone());
                }
                // Clear any miss count for resolved markets
                self.miss_counts.remove(market_id);
                continue;
            }

            // Check if this is a new market
            if !current_set.contains(market_id.as_str()) {
                debug!(
                    market_id = %market_id,
                    title = %market.title,
                    outcomes = market.outcomes.len(),
                    "Adding new market"
                );
                result.added.push(market.clone());
            }

            // Clear miss count for markets that exist in fresh data
            self.miss_counts.remove(market_id);
        }

        // Check for missing markets (hysteresis)
        for market_id in current_ids {
            if !fresh_ids.contains(market_id.as_str()) {
                let count = self.miss_counts.entry(market_id.clone()).or_insert(0);
                *count += 1;

                debug!(
                    market_id = %market_id,
                    miss_count = *count,
                    threshold = self.config.miss_threshold,
                    "Market missing from Gamma response"
                );

                if *count >= self.config.miss_threshold {
                    info!(
                        market_id = %market_id,
                        "Removing market after {} consecutive misses",
                        self.config.miss_threshold
                    );
                    result.removed.push(market_id.clone());
                    self.miss_counts.remove(market_id);
                }
            }
        }

        // Determine if we need to resubscribe (with debounce)
        let time_since_last = self.last_resubscribe.elapsed();
        if result.has_changes() && time_since_last >= self.config.resubscribe_debounce {
            result.needs_resubscribe = true;
            self.last_resubscribe = Instant::now();
            info!(
                added = result.added.len(),
                removed = result.removed.len(),
                "Triggering WS resubscribe"
            );
        } else if result.has_changes() {
            debug!(
                added = result.added.len(),
                removed = result.removed.len(),
                debounce_remaining_ms =
                    (self.config.resubscribe_debounce - time_since_last).as_millis(),
                "Changes detected but resubscribe debounced"
            );
        }

        result
    }

    /// Reset miss count for a specific market (e.g., when manually added)
    pub fn reset_miss_count(&mut self, market_id: &str) {
        self.miss_counts.remove(market_id);
    }

    /// Get current miss counts (for diagnostics)
    pub fn miss_counts(&self) -> &HashMap<String, u8> {
        &self.miss_counts
    }

    /// Force allow next resubscribe (bypass debounce)
    pub fn force_next_resubscribe(&mut self) {
        self.last_resubscribe = Instant::now() - Duration::from_secs(3600);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moonbag_core::models::StoredOutcome;

    fn make_market(id: &str, active: bool, closed: bool) -> StoredMarket {
        StoredMarket {
            neg_risk_market_id: id.to_string(),
            title: format!("Test Market {}", id),
            slug: id.to_string(),
            outcome_count: 2,
            outcomes: vec![StoredOutcome {
                question: "Yes?".to_string(),
                condition_id: "cond1".to_string(),
                yes_token_id: "yes1".to_string(),
                no_token_id: "no1".to_string(),
                alphabetical_index: 0,
            }],
            convertible_indices: vec![0],
            collected_at: chrono::Utc::now().to_rfc3339(),
            event_id: Some("event1".to_string()),
            active: Some(active),
            closed: Some(closed),
            liquidity: None,
            volume: None,
            volume24hr: None,
            open_interest: None,
            updated_at: None,
        }
    }

    #[test]
    fn test_add_new_market() {
        let mut reconciler = MarketReconciler::with_defaults();
        let current: Vec<String> = vec![];
        let fresh = vec![make_market("m1", true, false)];

        let result = reconciler.reconcile(&current, &fresh);
        assert_eq!(result.added.len(), 1);
        assert!(result.removed.is_empty());
    }

    #[test]
    fn test_remove_resolved_immediately() {
        let mut reconciler = MarketReconciler::with_defaults();
        let current = vec!["m1".to_string()];

        // Market comes back as closed
        let fresh = vec![make_market("m1", true, true)];

        let result = reconciler.reconcile(&current, &fresh);
        assert!(result.added.is_empty());
        assert_eq!(result.removed, vec!["m1"]);
    }

    #[test]
    fn test_remove_inactive_immediately() {
        let mut reconciler = MarketReconciler::with_defaults();
        let current = vec!["m1".to_string()];

        // Market comes back as inactive
        let fresh = vec![make_market("m1", false, false)];

        let result = reconciler.reconcile(&current, &fresh);
        assert!(result.added.is_empty());
        assert_eq!(result.removed, vec!["m1"]);
    }

    #[test]
    fn test_missing_market_hysteresis() {
        let config = ReconcilerConfig {
            miss_threshold: 3,
            resubscribe_debounce: Duration::from_millis(1), // Instant for testing
        };
        let mut reconciler = MarketReconciler::new(config);
        let current = vec!["m1".to_string()];
        let fresh: Vec<StoredMarket> = vec![]; // m1 is missing

        // Miss 1 - not removed yet
        let result = reconciler.reconcile(&current, &fresh);
        assert!(result.removed.is_empty());
        assert_eq!(*reconciler.miss_counts().get("m1").unwrap(), 1);

        // Miss 2 - not removed yet
        let result = reconciler.reconcile(&current, &fresh);
        assert!(result.removed.is_empty());
        assert_eq!(*reconciler.miss_counts().get("m1").unwrap(), 2);

        // Miss 3 - NOW removed
        let result = reconciler.reconcile(&current, &fresh);
        assert_eq!(result.removed, vec!["m1"]);
        assert!(!reconciler.miss_counts().contains_key("m1"));
    }

    #[test]
    fn test_miss_count_reset_on_return() {
        let config = ReconcilerConfig {
            miss_threshold: 3,
            resubscribe_debounce: Duration::from_millis(1),
        };
        let mut reconciler = MarketReconciler::new(config);
        let current = vec!["m1".to_string()];

        // Miss 1
        let fresh: Vec<StoredMarket> = vec![];
        let _ = reconciler.reconcile(&current, &fresh);
        assert_eq!(*reconciler.miss_counts().get("m1").unwrap(), 1);

        // Market returns - miss count should reset
        let fresh = vec![make_market("m1", true, false)];
        let _ = reconciler.reconcile(&current, &fresh);
        assert!(!reconciler.miss_counts().contains_key("m1"));
    }

    #[test]
    fn test_resubscribe_debounce() {
        let config = ReconcilerConfig {
            miss_threshold: 1,
            resubscribe_debounce: Duration::from_secs(10), // Long debounce
        };
        let mut reconciler = MarketReconciler::new(config);

        // First change - should trigger resubscribe
        let current: Vec<String> = vec![];
        let fresh = vec![make_market("m1", true, false)];
        let result = reconciler.reconcile(&current, &fresh);
        assert!(result.needs_resubscribe);

        // Second change immediately after - should be debounced
        let current = vec!["m1".to_string()];
        let fresh = vec![
            make_market("m1", true, false),
            make_market("m2", true, false),
        ];
        let result = reconciler.reconcile(&current, &fresh);
        assert!(!result.needs_resubscribe); // Debounced!
        assert!(!result.added.is_empty()); // But still has changes
    }
}
