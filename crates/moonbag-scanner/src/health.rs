//! Health HTTP server for 24/7 scanner operation.
//!
//! Endpoints:
//! - `GET /health` -> always 200 + JSON status payload
//! - `GET /ready`  -> 200 if ready, 503 otherwise
//! - `GET /metrics` -> Prometheus text format (optional)

use moonbag_core::{MoonbagError, Result};
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

const MAX_REQ_BYTES: usize = 8192;
const READY_WS_STALE_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HealthMode {
    Ws = 0,
    Rest = 1,
    Fallback = 2,
}

impl HealthMode {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Rest,
            2 => Self::Fallback,
            _ => Self::Ws,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ws => "ws",
            Self::Rest => "rest",
            Self::Fallback => "fallback",
        }
    }
}

pub struct HealthServer {
    bind: SocketAddr,
    state: Arc<HealthState>,
}

impl HealthServer {
    pub fn new(bind: &str, state: Arc<HealthState>) -> Result<Self> {
        let bind: SocketAddr = bind.parse().map_err(|e| {
            MoonbagError::InvalidConfig(format!("invalid health bind `{}`: {}", bind, e))
        })?;
        Ok(Self { bind, state })
    }

    pub fn state(&self) -> Arc<HealthState> {
        self.state.clone()
    }

    pub async fn run(self) -> Result<()> {
        let listener = TcpListener::bind(self.bind).await?;
        info!(bind = %self.bind, "Health server listening");

        loop {
            let (stream, peer) = listener.accept().await?;
            let state = self.state.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, state).await {
                    debug!(peer = %peer, error = %e, "Health connection error");
                }
            });
        }
    }
}

pub struct HealthState {
    pub uptime_start: Instant,
    mode: AtomicU8,

    pub ws_connected: AtomicBool,
    pub ws_fail_streak: AtomicU32,
    pub ws_reconnect_count: AtomicU64,
    pub last_ws_msg: AtomicU64,

    pub last_gamma_ok: AtomicU64,
    pub gamma_degraded: AtomicBool,

    pub markets_active: AtomicUsize,
    pub tokens_subscribed: AtomicUsize,

    pub opportunities_found: AtomicU64,
    pub dropped_updates: AtomicU64,
}

impl Default for HealthState {
    fn default() -> Self {
        Self {
            uptime_start: Instant::now(),
            mode: AtomicU8::new(HealthMode::Ws as u8),
            ws_connected: AtomicBool::new(false),
            ws_fail_streak: AtomicU32::new(0),
            ws_reconnect_count: AtomicU64::new(0),
            last_ws_msg: AtomicU64::new(0),
            last_gamma_ok: AtomicU64::new(0),
            gamma_degraded: AtomicBool::new(false),
            markets_active: AtomicUsize::new(0),
            tokens_subscribed: AtomicUsize::new(0),
            opportunities_found: AtomicU64::new(0),
            dropped_updates: AtomicU64::new(0),
        }
    }
}

impl HealthState {
    pub fn set_mode(&self, mode: HealthMode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    pub fn mode(&self) -> HealthMode {
        HealthMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    pub fn record_ws_msg(&self) {
        self.last_ws_msg.store(now_unix(), Ordering::Relaxed);
    }

    pub fn record_gamma_ok(&self) {
        self.last_gamma_ok.store(now_unix(), Ordering::Relaxed);
        self.gamma_degraded.store(false, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HealthResponse {
        let ws_connected = self.ws_connected.load(Ordering::Relaxed);
        let ws_fail_streak = self.ws_fail_streak.load(Ordering::Relaxed);
        let gamma_degraded = self.gamma_degraded.load(Ordering::Relaxed);

        let status = if ws_fail_streak > 5 && gamma_degraded {
            "unhealthy"
        } else if ws_connected && !gamma_degraded {
            "ok"
        } else {
            "degraded"
        };

        HealthResponse {
            status,
            uptime_secs: self.uptime_start.elapsed().as_secs(),
            mode: self.mode().as_str(),

            ws_connected,
            ws_fail_streak,
            ws_reconnect_count: self.ws_reconnect_count.load(Ordering::Relaxed),
            last_ws_msg_secs_ago: secs_ago(self.last_ws_msg.load(Ordering::Relaxed)),

            gamma_degraded,
            last_gamma_ok_secs_ago: secs_ago(self.last_gamma_ok.load(Ordering::Relaxed)),

            markets_active: self.markets_active.load(Ordering::Relaxed),
            tokens_subscribed: self.tokens_subscribed.load(Ordering::Relaxed),
            opportunities_found: self.opportunities_found.load(Ordering::Relaxed),
            dropped_updates: self.dropped_updates.load(Ordering::Relaxed),
        }
    }

    pub fn readiness(&self) -> (bool, Option<String>) {
        if !self.ws_connected.load(Ordering::Relaxed) {
            return (false, Some("ws_disconnected".to_string()));
        }

        if self.markets_active.load(Ordering::Relaxed) == 0 {
            return (false, Some("no_markets".to_string()));
        }

        let Some(age) = secs_ago(self.last_ws_msg.load(Ordering::Relaxed)) else {
            return (false, Some("ws_stale".to_string()));
        };

        if age > READY_WS_STALE_SECS {
            return (false, Some("ws_stale".to_string()));
        }

        (true, None)
    }

    pub fn metrics(&self) -> String {
        let uptime = self.uptime_start.elapsed().as_secs();
        let ws_connected = if self.ws_connected.load(Ordering::Relaxed) {
            1
        } else {
            0
        };
        let gamma_degraded = if self.gamma_degraded.load(Ordering::Relaxed) {
            1
        } else {
            0
        };
        let ws_fail_streak = self.ws_fail_streak.load(Ordering::Relaxed);
        let ws_reconnect_count = self.ws_reconnect_count.load(Ordering::Relaxed);
        let markets_active = self.markets_active.load(Ordering::Relaxed);
        let tokens_subscribed = self.tokens_subscribed.load(Ordering::Relaxed);
        let opportunities_found = self.opportunities_found.load(Ordering::Relaxed);
        let dropped_updates = self.dropped_updates.load(Ordering::Relaxed);

        let mut out = String::new();
        out.push_str("# TYPE moonbag_uptime_seconds counter\n");
        out.push_str(&format!("moonbag_uptime_seconds {}\n", uptime));
        out.push_str("# TYPE moonbag_ws_connected gauge\n");
        out.push_str(&format!("moonbag_ws_connected {}\n", ws_connected));
        out.push_str("# TYPE moonbag_ws_fail_streak gauge\n");
        out.push_str(&format!("moonbag_ws_fail_streak {}\n", ws_fail_streak));
        out.push_str("# TYPE moonbag_ws_reconnect_count counter\n");
        out.push_str(&format!(
            "moonbag_ws_reconnect_count {}\n",
            ws_reconnect_count
        ));
        out.push_str("# TYPE moonbag_gamma_degraded gauge\n");
        out.push_str(&format!("moonbag_gamma_degraded {}\n", gamma_degraded));
        out.push_str("# TYPE moonbag_markets_active gauge\n");
        out.push_str(&format!("moonbag_markets_active {}\n", markets_active));
        out.push_str("# TYPE moonbag_tokens_subscribed gauge\n");
        out.push_str(&format!(
            "moonbag_tokens_subscribed {}\n",
            tokens_subscribed
        ));
        out.push_str("# TYPE moonbag_opportunities_found_total counter\n");
        out.push_str(&format!(
            "moonbag_opportunities_found_total {}\n",
            opportunities_found
        ));
        out.push_str("# TYPE moonbag_dropped_updates_total counter\n");
        out.push_str(&format!(
            "moonbag_dropped_updates_total {}\n",
            dropped_updates
        ));

        if let Some(age) = secs_ago(self.last_ws_msg.load(Ordering::Relaxed)) {
            out.push_str("# TYPE moonbag_last_ws_msg_seconds_ago gauge\n");
            out.push_str(&format!("moonbag_last_ws_msg_seconds_ago {}\n", age));
        }
        if let Some(age) = secs_ago(self.last_gamma_ok.load(Ordering::Relaxed)) {
            out.push_str("# TYPE moonbag_last_gamma_ok_seconds_ago gauge\n");
            out.push_str(&format!("moonbag_last_gamma_ok_seconds_ago {}\n", age));
        }

        out
    }
}

#[derive(Serialize)]
pub struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
    mode: &'static str,

    ws_connected: bool,
    ws_fail_streak: u32,
    ws_reconnect_count: u64,
    last_ws_msg_secs_ago: Option<u64>,

    gamma_degraded: bool,
    last_gamma_ok_secs_ago: Option<u64>,

    markets_active: usize,
    tokens_subscribed: usize,
    opportunities_found: u64,
    dropped_updates: u64,
}

#[derive(Serialize)]
pub struct ReadyResponse {
    ready: bool,
    reason: Option<String>,
}

async fn handle_connection(mut stream: TcpStream, state: Arc<HealthState>) -> Result<()> {
    let mut buf = [0u8; MAX_REQ_BYTES];
    let mut read_len = 0usize;

    loop {
        let n = stream.read(&mut buf[read_len..]).await?;
        if n == 0 {
            break;
        }
        read_len += n;

        if read_len >= 4 && buf[..read_len].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if read_len == MAX_REQ_BYTES {
            warn!("Health request exceeded {} bytes", MAX_REQ_BYTES);
            break;
        }
    }

    if read_len == 0 {
        return Ok(());
    }

    let req = std::str::from_utf8(&buf[..read_len]).unwrap_or("");
    let request_line = req.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();

    let method = parts.next().unwrap_or("");
    let mut path = parts.next().unwrap_or("");
    path = path.split('?').next().unwrap_or(path);

    match (method, path) {
        ("GET", "/health") => {
            let body = serde_json::to_vec(&state.snapshot())?;
            write_response(&mut stream, 200, "application/json", &body).await?;
        }
        ("GET", "/ready") => {
            let (ready, reason) = state.readiness();
            let body = serde_json::to_vec(&ReadyResponse { ready, reason })?;
            let status = if ready { 200 } else { 503 };
            write_response(&mut stream, status, "application/json", &body).await?;
        }
        ("GET", "/metrics") => {
            let body = state.metrics();
            write_response(
                &mut stream,
                200,
                "text/plain; version=0.0.4",
                body.as_bytes(),
            )
            .await?;
        }
        ("GET", _) => {
            write_response(&mut stream, 404, "text/plain", b"not found").await?;
        }
        _ => {
            write_response(&mut stream, 405, "text/plain", b"method not allowed").await?;
        }
    }

    Ok(())
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "OK",
    };

    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        reason,
        content_type,
        body.len()
    );

    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn secs_ago(ts: u64) -> Option<u64> {
    if ts == 0 {
        return None;
    }
    now_unix().checked_sub(ts)
}
