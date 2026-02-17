//! WebSocket client for Polymarket CLOB
//!
//! Connects to the Polymarket WebSocket feed and processes orderbook updates.

use crate::orderbook::{BookSide, OrderbookManager};
use futures::{SinkExt, StreamExt};
use moonbag_core::models::PriceLevel;
use moonbag_core::{MoonbagError, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, Message},
};
use tracing::{debug, error, info, trace, warn};

/// Polymarket WebSocket URL
pub const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// Subscription message
#[derive(Debug, Serialize)]
struct SubscribeMessage {
    assets_ids: Vec<String>,
    channel: String,
    #[serde(rename = "type")]
    msg_type: String,
}

/// Book event from WebSocket
#[derive(Debug, Deserialize)]
struct BookEvent {
    event_type: Option<String>,
    asset_id: Option<String>,
    bids: Option<Vec<BookLevel>>,
    asks: Option<Vec<BookLevel>>,
    /// Server timestamp in milliseconds (Unix epoch)
    timestamp: Option<String>,
}

/// Price change from WebSocket
#[derive(Debug, Deserialize)]
struct PriceChangeEvent {
    asset_id: Option<String>,
    price: Option<String>,
    size: Option<String>,
    side: Option<String>,
    best_bid: Option<String>,
    best_ask: Option<String>,
}

/// Price changes wrapper
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PriceChangesMessage {
    market: Option<String>,
    price_changes: Option<Vec<PriceChangeEvent>>,
    /// Server timestamp in milliseconds (Unix epoch)
    timestamp: Option<String>,
    event_type: Option<String>,
}

/// Book level from WS (string prices/sizes)
#[derive(Debug, Deserialize)]
struct BookLevel {
    price: Option<String>,
    size: Option<String>,
}

impl BookLevel {
    fn to_price_level(&self) -> Option<PriceLevel> {
        let price = Decimal::from_str(self.price.as_ref()?).ok()?;
        let size = Decimal::from_str(self.size.as_ref()?).ok()?;
        if size <= Decimal::ZERO {
            return None;
        }
        Some((price, size))
    }
}

/// Type of WebSocket update (for differentiated cooldown)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsUpdateKind {
    /// Full orderbook snapshot (has depth - valuable for VWAP)
    Book,
    /// Price change only (quote-only, no size - not useful for analyzer)
    PriceChange,
}

/// Message types that can be sent through the channel
#[derive(Debug)]
pub enum WsUpdate {
    /// Token was updated (orderbook already modified in-place)
    Token {
        token_id: String,
        /// Type of update (Book vs PriceChange)
        kind: WsUpdateKind,
        /// When the WS message was received (for latency measurement)
        received_at: std::time::Instant,
        /// Server timestamp in milliseconds (from Polymarket)
        server_ts_ms: Option<u64>,
    },
    /// Connection status
    Connected,
    Disconnected,
}

/// Control messages for managing the WS connection lifecycle.
#[derive(Debug)]
pub enum WsControl {
    /// Close current connection and reconnect with updated tokens.
    Resubscribe,
    /// Shutdown the WS client loop.
    Shutdown,
}

/// WebSocket client configuration
#[derive(Debug, Clone)]
pub struct WsClientConfig {
    /// Batch size for subscriptions
    pub subscribe_batch_size: usize,
    /// Delay between subscription batches
    pub subscribe_batch_delay: Duration,
    /// Reconnect delay (will exponential backoff)
    pub reconnect_delay: Duration,
    /// Max reconnect delay
    pub max_reconnect_delay: Duration,
    /// Ping interval
    pub ping_interval: Duration,
    /// Idle timeout (no incoming messages) before reconnecting
    pub idle_timeout: Duration,
}

impl Default for WsClientConfig {
    fn default() -> Self {
        Self {
            subscribe_batch_size: 500,
            subscribe_batch_delay: Duration::from_millis(100),
            reconnect_delay: Duration::from_secs(1),
            max_reconnect_delay: Duration::from_secs(60),
            ping_interval: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// WebSocket client for Polymarket orderbook updates
pub struct WsClient {
    config: WsClientConfig,
    orderbook: Arc<OrderbookManager>,
    message_count: AtomicU64,
    price_change_updates: AtomicU64,
    book_updates: AtomicU64,
    dropped_count: AtomicU64,
}

impl WsClient {
    /// Create a new WebSocket client
    pub fn new(orderbook: Arc<OrderbookManager>, config: WsClientConfig) -> Self {
        Self {
            config,
            orderbook,
            message_count: AtomicU64::new(0),
            price_change_updates: AtomicU64::new(0),
            book_updates: AtomicU64::new(0),
            dropped_count: AtomicU64::new(0),
        }
    }

    /// Create with default config
    pub fn with_orderbook(orderbook: Arc<OrderbookManager>) -> Self {
        Self::new(orderbook, WsClientConfig::default())
    }

    /// Get message count
    pub fn message_count(&self) -> u64 {
        self.message_count.load(Ordering::Relaxed)
    }

    /// Get dropped message count
    pub fn dropped_count(&self) -> u64 {
        self.dropped_count.load(Ordering::Relaxed)
    }

    /// Count of per-token `price_changes` updates processed (quote-only).
    pub fn price_change_updates(&self) -> u64 {
        self.price_change_updates.load(Ordering::Relaxed)
    }

    /// Count of per-token `book` updates processed (depth).
    pub fn book_updates(&self) -> u64 {
        self.book_updates.load(Ordering::Relaxed)
    }

    /// Run the WebSocket client with reconnection logic
    ///
    /// This will:
    /// 1. Connect to WebSocket
    /// 2. Subscribe to all tokens
    /// 3. Process messages and update orderbook
    /// 4. Reconnect on disconnect
    ///
    /// Sends updates through the provided channel for the scanner to process.
    pub async fn run(
        &self,
        tokens: Vec<String>,
        update_tx: mpsc::Sender<WsUpdate>,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<()> {
        let mut reconnect_delay = self.config.reconnect_delay;

        loop {
            tokio::select! {
                _ = shutdown.recv() => {
                    info!("WebSocket client shutting down");
                    return Ok(());
                }
                result = self.connect_and_run(&tokens, &update_tx) => {
                    let is_error = match result {
                        Ok(()) => {
                            info!("WebSocket disconnected gracefully");
                            // Reset backoff after successful run
                            reconnect_delay = self.config.reconnect_delay;
                            false
                        }
                        Err(e) => {
                            warn!("WebSocket error: {}, reconnecting in {:?}", e, reconnect_delay);
                            true
                        }
                    };

                    let _ = update_tx.send(WsUpdate::Disconnected).await;

                    // Exponential backoff on errors only
                    tokio::time::sleep(reconnect_delay).await;
                    if is_error {
                        reconnect_delay = (reconnect_delay * 2).min(self.config.max_reconnect_delay);
                    }
                }
            }
        }
    }

    /// Run the WebSocket client with a control channel for dynamic resubscribe.
    ///
    /// - Subscribes to `orderbook.active_token_ids()` on each connection
    /// - `WsControl::Resubscribe` triggers an immediate reconnect (coalesced)
    /// - Heartbeat pings + idle timeout improve 24/7 stability
    pub async fn run_with_control(
        &self,
        update_tx: mpsc::Sender<WsUpdate>,
        mut control_rx: mpsc::Receiver<WsControl>,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<()> {
        let mut reconnect_delay = self.config.reconnect_delay;

        loop {
            // Always subscribe to current active tokens
            let tokens = self.orderbook.active_token_ids();
            if tokens.is_empty() {
                // Avoid busy loop while no tokens; wait for control/shutdown
                tokio::select! {
                    _ = shutdown.recv() => {
                        info!("WebSocket client shutting down");
                        return Ok(());
                    }
                    ctrl = control_rx.recv() => {
                        match ctrl {
                            Some(WsControl::Resubscribe) => continue,
                            Some(WsControl::Shutdown) | None => {
                                info!("WebSocket client shutting down");
                                return Ok(());
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        continue;
                    }
                }
            }

            info!("Connecting to WebSocket: {}", WS_URL);

            let (ws_stream, _) = match connect_async(WS_URL).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        "WebSocket connect error: {}, reconnecting in {:?}",
                        e, reconnect_delay
                    );

                    tokio::select! {
                        _ = shutdown.recv() => {
                            info!("WebSocket client shutting down");
                            return Ok(());
                        }
                        ctrl = control_rx.recv() => {
                            match ctrl {
                                Some(WsControl::Resubscribe) => continue,
                                Some(WsControl::Shutdown) | None => {
                                    info!("WebSocket client shutting down");
                                    return Ok(());
                                }
                            }
                        }
                        _ = tokio::time::sleep(reconnect_delay) => {}
                    }

                    reconnect_delay = (reconnect_delay * 2).min(self.config.max_reconnect_delay);
                    continue;
                }
            };

            info!("WebSocket connected");
            let _ = update_tx.send(WsUpdate::Connected).await;

            let (mut write, mut read) = ws_stream.split();

            // Subscribe in batches
            let batch_count = (tokens.len() + self.config.subscribe_batch_size - 1)
                / self.config.subscribe_batch_size;

            info!(
                "Subscribing to {} tokens in {} batches",
                tokens.len(),
                batch_count
            );

            let mut subscribe_error: Option<MoonbagError> = None;
            for (i, batch) in tokens.chunks(self.config.subscribe_batch_size).enumerate() {
                let sub_msg = SubscribeMessage {
                    assets_ids: batch.to_vec(),
                    channel: "market".to_string(),
                    msg_type: "subscribe".to_string(),
                };

                let msg_json = match serde_json::to_string(&sub_msg) {
                    Ok(v) => v,
                    Err(e) => {
                        subscribe_error = Some(e.into());
                        break;
                    }
                };

                if let Err(e) = write.send(Message::Text(msg_json.into())).await {
                    subscribe_error = Some(MoonbagError::WebSocketConnection(format!(
                        "Subscribe: {}",
                        e
                    )));
                    break;
                }

                if i < batch_count - 1 {
                    tokio::time::sleep(self.config.subscribe_batch_delay).await;
                }

                debug!("Subscribed batch {}/{}", i + 1, batch_count);
            }

            if let Some(err) = subscribe_error {
                warn!(
                    "WebSocket subscribe error: {}, reconnecting in {:?}",
                    err, reconnect_delay
                );
                let _ = update_tx.send(WsUpdate::Disconnected).await;

                tokio::select! {
                    _ = shutdown.recv() => {
                        info!("WebSocket client shutting down");
                        return Ok(());
                    }
                    ctrl = control_rx.recv() => {
                        match ctrl {
                            Some(WsControl::Resubscribe) => continue,
                            Some(WsControl::Shutdown) | None => {
                                info!("WebSocket client shutting down");
                                return Ok(());
                            }
                        }
                    }
                    _ = tokio::time::sleep(reconnect_delay) => {}
                }

                reconnect_delay = (reconnect_delay * 2).min(self.config.max_reconnect_delay);
                continue;
            }

            info!("Subscription complete, processing messages...");

            // Reset backoff after successful connect+subscribe
            reconnect_delay = self.config.reconnect_delay;

            let mut ping_ticker = tokio::time::interval(self.config.ping_interval);
            ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            let idle_sleep = tokio::time::sleep(self.config.idle_timeout);
            tokio::pin!(idle_sleep);

            let mut resubscribe_pending = false;
            let mut is_error = false;

            // Message loop with ping + idle timeout + control
            loop {
                tokio::select! {
                    _ = shutdown.recv() => {
                        info!("WebSocket client shutting down");
                        return Ok(());
                    }

                    ctrl = control_rx.recv() => {
                        match ctrl {
                            Some(WsControl::Resubscribe) => {
                                resubscribe_pending = true;
                                break;
                            }
                            Some(WsControl::Shutdown) | None => {
                                info!("WebSocket client shutting down");
                                return Ok(());
                            }
                        }
                    }

                    _ = ping_ticker.tick() => {
                        if let Err(e) = write.send(Message::Ping(Vec::new().into())).await {
                            warn!("Failed to send ping: {}", e);
                            is_error = true;
                            break;
                        }
                    }

                    _ = &mut idle_sleep => {
                        warn!("WS idle timeout, reconnecting");
                        is_error = true;
                        break;
                    }

                    msg = read.next() => {
                        let Some(msg_result) = msg else {
                            break;
                        };

                        // Reset idle timer on ANY incoming frame
                        idle_sleep.as_mut().reset(tokio::time::Instant::now() + self.config.idle_timeout);

                        let msg = match msg_result {
                            Ok(m) => m,
                            Err(e) => {
                                error!("WebSocket read error: {}", e);
                                is_error = true;
                                break;
                            }
                        };

                        match msg {
                            Message::Text(text) => {
                                self.process_message(&text, &update_tx);
                            }
                            Message::Ping(data) => {
                                trace!("Received ping");
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Message::Close(frame) => {
                                info!("WebSocket closed: {:?}", frame);
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }

            let _ = update_tx.send(WsUpdate::Disconnected).await;

            // Coalesce multiple Resubscribe requests
            while let Ok(ctrl) = control_rx.try_recv() {
                match ctrl {
                    WsControl::Resubscribe => resubscribe_pending = true,
                    WsControl::Shutdown => {
                        info!("WebSocket client shutting down");
                        return Ok(());
                    }
                }
            }

            if resubscribe_pending {
                info!("Resubscribing with updated token list");
                let _ = write.send(Message::Close(None)).await;
                continue;
            }

            if !is_error {
                reconnect_delay = self.config.reconnect_delay;
            }

            // Exponential backoff; allow shutdown/control to interrupt sleep
            tokio::select! {
                _ = shutdown.recv() => {
                    info!("WebSocket client shutting down");
                    return Ok(());
                }
                ctrl = control_rx.recv() => {
                    match ctrl {
                        Some(WsControl::Resubscribe) => continue,
                        Some(WsControl::Shutdown) | None => {
                            info!("WebSocket client shutting down");
                            return Ok(());
                        }
                    }
                }
                _ = tokio::time::sleep(reconnect_delay) => {}
            }

            if is_error {
                reconnect_delay = (reconnect_delay * 2).min(self.config.max_reconnect_delay);
            }
        }
    }

    /// Single connection attempt
    async fn connect_and_run(
        &self,
        tokens: &[String],
        update_tx: &mpsc::Sender<WsUpdate>,
    ) -> Result<()> {
        info!("Connecting to WebSocket: {}", WS_URL);

        let (ws_stream, _) = connect_async(WS_URL)
            .await
            .map_err(|e| MoonbagError::WebSocketConnection(format!("{}", e)))?;

        info!("WebSocket connected");
        let _ = update_tx.send(WsUpdate::Connected).await;

        let (mut write, mut read) = ws_stream.split();

        // Subscribe in batches
        let batches = tokens.chunks(self.config.subscribe_batch_size);
        let batch_count = (tokens.len() + self.config.subscribe_batch_size - 1)
            / self.config.subscribe_batch_size;

        info!(
            "Subscribing to {} tokens in {} batches",
            tokens.len(),
            batch_count
        );

        for (i, batch) in batches.enumerate() {
            let sub_msg = SubscribeMessage {
                assets_ids: batch.to_vec(),
                channel: "market".to_string(),
                msg_type: "subscribe".to_string(),
            };

            let msg_json = serde_json::to_string(&sub_msg)?;
            write
                .send(Message::Text(msg_json.into()))
                .await
                .map_err(|e| MoonbagError::WebSocketConnection(format!("Subscribe: {}", e)))?;

            if i < batch_count - 1 {
                tokio::time::sleep(self.config.subscribe_batch_delay).await;
            }

            debug!("Subscribed batch {}/{}", i + 1, batch_count);
        }

        info!("Subscription complete, processing messages...");

        // Reset reconnect delay on successful connection
        // (Note: This is handled by the caller)

        // Message processing loop
        loop {
            let msg: Option<std::result::Result<Message, tungstenite::Error>> = read.next().await;
            let Some(msg_result) = msg else {
                break;
            };

            let msg = match msg_result {
                Ok(m) => m,
                Err(e) => {
                    error!("WebSocket read error: {}", e);
                    return Err(MoonbagError::WebSocketConnection(format!("Read: {}", e)));
                }
            };

            match msg {
                Message::Text(text) => {
                    self.process_message(&text, update_tx);
                }
                Message::Ping(data) => {
                    trace!("Received ping");
                    let pong_result: std::result::Result<(), tungstenite::Error> =
                        write.send(Message::Pong(data)).await;
                    if pong_result.is_err() {
                        warn!("Failed to send pong");
                    }
                }
                Message::Close(frame) => {
                    info!("WebSocket closed: {:?}", frame);
                    return Ok(());
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Process a single WebSocket message.
    ///
    /// The feed uses multiple formats:
    /// - `[` ... `]` : book snapshot(s) (may omit `event_type`)
    /// - `{ "price_changes": [...] }` : price-level deltas + best bid/ask (`event_type=price_change`)
    /// - `{ "asset_id": "...", "bids": [...], "asks": [...] }` : single-token book snapshot
    fn process_message(&self, text: &str, update_tx: &mpsc::Sender<WsUpdate>) {
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return;
        }

        self.message_count.fetch_add(1, Ordering::Relaxed);

        // First-byte routing
        let first_byte = bytes.iter().find(|&&b| !b.is_ascii_whitespace()).copied();

        match first_byte {
            Some(b'{') => self.process_object_message(text, update_tx),
            Some(b'[') => self.process_array_message(text, update_tx),
            _ => {}
        };
    }

    #[inline]
    fn process_object_message(&self, text: &str, update_tx: &mpsc::Sender<WsUpdate>) {
        // 1) price_changes wrapper (deltas + best bid/ask)
        if let Ok(price_msg) = serde_json::from_str::<PriceChangesMessage>(text) {
            if let Some(changes) = price_msg.price_changes {
                let server_ts_ms = price_msg.timestamp.as_ref().and_then(|s| s.parse::<u64>().ok());

                for change in changes {
                    let Some(token_id) = change.asset_id else {
                        continue;
                    };

                    // Always update best prices if present (useful for debug/telemetry).
                    let best_bid = change
                        .best_bid
                        .as_ref()
                        .and_then(|s| Decimal::from_str(s).ok());
                    let best_ask = change
                        .best_ask
                        .as_ref()
                        .and_then(|s| Decimal::from_str(s).ok());
                    if best_bid.is_some() || best_ask.is_some() {
                        self.orderbook
                            .update_best_prices(&token_id, best_bid, best_ask);
                    }

                    // If we have a full delta (price+size+side), apply it to depth.
                    let delta_side = change.side.as_deref().and_then(|s| {
                        if s.eq_ignore_ascii_case("BUY") || s.eq_ignore_ascii_case("BID") {
                            Some(BookSide::Bid)
                        } else if s.eq_ignore_ascii_case("SELL") || s.eq_ignore_ascii_case("ASK") {
                            Some(BookSide::Ask)
                        } else {
                            None
                        }
                    });

                    let delta_price = change
                        .price
                        .as_ref()
                        .and_then(|s| Decimal::from_str(s).ok());
                    let delta_size = change
                        .size
                        .as_ref()
                        .and_then(|s| Decimal::from_str(s).ok());

                    let (kind, server_ts_for_update) = if let (Some(side), Some(price), Some(size)) =
                        (delta_side, delta_price, delta_size)
                    {
                        self.orderbook.update_price_level(&token_id, side, price, size);
                        self.book_updates.fetch_add(1, Ordering::Relaxed);
                        (WsUpdateKind::Book, server_ts_ms)
                    } else {
                        self.price_change_updates.fetch_add(1, Ordering::Relaxed);
                        (WsUpdateKind::PriceChange, None)
                    };

                    // Notify scanner (no data cloning)
                    let send_res = update_tx.try_send(WsUpdate::Token {
                        token_id,
                        kind,
                        received_at: std::time::Instant::now(),
                        server_ts_ms: server_ts_for_update,
                    });
                    if let Err(err) = send_res {
                        match err {
                            TrySendError::Full(_) => {
                                self.dropped_count.fetch_add(1, Ordering::Relaxed);
                            }
                            TrySendError::Closed(_) => {}
                        }
                    }
                }

                return;
            }
        }

        // 2) single-token book snapshot (dict format)
        let Ok(event) = serde_json::from_str::<BookEvent>(text) else {
            return;
        };

        // Some feeds omit `event_type` for book snapshots. If present, only accept `book`.
        if let Some(et) = event.event_type.as_deref() {
            if et != "book" {
                return;
            }
        }

        let Some(token_id) = event.asset_id else {
            return;
        };

        // Guard: don't clobber depth with non-book object payloads.
        if event.bids.is_none() && event.asks.is_none() {
            return;
        }

        let bids: Vec<PriceLevel> = event
            .bids
            .unwrap_or_default()
            .iter()
            .filter_map(|l| l.to_price_level())
            .collect();

        let asks: Vec<PriceLevel> = event
            .asks
            .unwrap_or_default()
            .iter()
            .filter_map(|l| l.to_price_level())
            .collect();

        let server_ts_ms = event.timestamp.as_ref().and_then(|s| s.parse::<u64>().ok());

        self.orderbook.update_orderbook(&token_id, bids, asks);
        self.book_updates.fetch_add(1, Ordering::Relaxed);

        let send_res = update_tx.try_send(WsUpdate::Token {
            token_id,
            kind: WsUpdateKind::Book,
            received_at: std::time::Instant::now(),
            server_ts_ms,
        });
        if let Err(err) = send_res {
            match err {
                TrySendError::Full(_) => {
                    self.dropped_count.fetch_add(1, Ordering::Relaxed);
                }
                TrySendError::Closed(_) => {}
            }
        }
    }

    #[inline]
    fn process_array_message(&self, text: &str, update_tx: &mpsc::Sender<WsUpdate>) {
        // book events (array format)
        let Ok(events) = serde_json::from_str::<Vec<BookEvent>>(text) else {
            return;
        };

        for event in events {
            // Some feeds omit `event_type` for book snapshots. If present, only accept `book`.
            if let Some(et) = event.event_type.as_deref() {
                if et != "book" {
                    continue;
                }
            }

            let Some(token_id) = event.asset_id else {
                continue;
            };

            // Guard: don't clobber depth with non-book object payloads.
            if event.bids.is_none() && event.asks.is_none() {
                continue;
            }

            let bids: Vec<PriceLevel> = event
                .bids
                .unwrap_or_default()
                .iter()
                .filter_map(|l| l.to_price_level())
                .collect();

            let asks: Vec<PriceLevel> = event
                .asks
                .unwrap_or_default()
                .iter()
                .filter_map(|l| l.to_price_level())
                .collect();

            // Parse server timestamp
            let server_ts_ms = event.timestamp.as_ref().and_then(|s| s.parse::<u64>().ok());

            // Update orderbook directly
            self.orderbook.update_orderbook(&token_id, bids, asks);
            self.book_updates.fetch_add(1, Ordering::Relaxed);

            // Notify scanner (no data cloning)
            let send_res = update_tx.try_send(WsUpdate::Token {
                token_id,
                kind: WsUpdateKind::Book,
                received_at: std::time::Instant::now(),
                server_ts_ms,
            });
            if let Err(err) = send_res {
                match err {
                    TrySendError::Full(_) => {
                        self.dropped_count.fetch_add(1, Ordering::Relaxed);
                    }
                    TrySendError::Closed(_) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_book_level_parsing() {
        let level = BookLevel {
            price: Some("0.45".to_string()),
            size: Some("100.5".to_string()),
        };
        let parsed = level.to_price_level().unwrap();
        assert_eq!(parsed.0, Decimal::new(45, 2));
        assert_eq!(parsed.1, Decimal::new(1005, 1));
    }

    #[test]
    fn test_book_level_zero_size() {
        let level = BookLevel {
            price: Some("0.45".to_string()),
            size: Some("0".to_string()),
        };
        assert!(level.to_price_level().is_none());
    }
}
