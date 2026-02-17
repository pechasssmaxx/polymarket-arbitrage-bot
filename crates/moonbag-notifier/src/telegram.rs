//! Telegram notification client
//!
//! Sends opportunity alerts via Telegram Bot API

use moonbag_core::models::MoonbagOpportunity;
use moonbag_core::Result;
use rust_decimal::Decimal;
use serde::Serialize;
use tracing::{debug, error, info};

const TELEGRAM_API: &str = "https://api.telegram.org";

/// Request body for sendMessage
#[derive(Debug, Serialize)]
struct SendMessageRequest<'a> {
    chat_id: &'a str,
    text: &'a str,
    parse_mode: &'a str,
    disable_web_page_preview: bool,
}

/// Telegram notifier
pub struct TelegramNotifier {
    bot_token: String,
    chat_id: String,
    enabled: bool,
    client: reqwest::Client,
}

impl TelegramNotifier {
    /// Create a new Telegram notifier
    pub fn new(bot_token: Option<String>, chat_id: Option<String>) -> Self {
        let enabled = bot_token.is_some() && chat_id.is_some();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            bot_token: bot_token.unwrap_or_default(),
            chat_id: chat_id.unwrap_or_default(),
            enabled,
            client,
        }
    }

    /// Create from environment variables
    pub fn from_env() -> Self {
        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let chat_id = std::env::var("TELEGRAM_CHAT_ID")
            .ok()
            .filter(|s| !s.is_empty());
        Self::new(bot_token, chat_id)
    }

    /// Check if notifier is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Send a raw text message
    pub async fn send_message(&self, text: &str) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }

        let url = format!("{}/bot{}/sendMessage", TELEGRAM_API, self.bot_token);

        let request = SendMessageRequest {
            chat_id: &self.chat_id,
            text,
            parse_mode: "HTML",
            disable_web_page_preview: true,
        };

        let response = self.client.post(&url).json(&request).send().await;

        match response {
            Ok(resp) => {
                if resp.status().is_success() {
                    debug!("Telegram message sent successfully");
                    Ok(true)
                } else {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    error!("Telegram API error: {} - {}", status, body);
                    Ok(false)
                }
            }
            Err(e) => {
                error!("Failed to send Telegram message: {}", e);
                Ok(false)
            }
        }
    }

    /// Send an opportunity notification
    pub async fn send_opportunity(&self, opp: &MoonbagOpportunity) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }

        let message = format_opportunity_message(opp);
        info!(
            market = %opp.market_title,
            profit = %opp.guaranteed_profit,
            "Sending Telegram notification"
        );

        self.send_message(&message).await
    }

    /// Send a test message to verify connection
    pub async fn send_test(&self) -> Result<bool> {
        self.send_message("Moonbag Scanner: Connection test successful!")
            .await
    }
}

/// Format an opportunity as a Telegram message
fn format_opportunity_message(opp: &MoonbagOpportunity) -> String {
    let mut msg = String::new();

    // Header with profit
    msg.push_str(&format!("<b>MOONBAG OPPORTUNITY</b>\n\n"));

    msg.push_str(&format!("<b>Profit: ${:.4}</b>\n", opp.guaranteed_profit));

    msg.push_str(&format!("Market: {}\n", html_escape(&opp.market_title)));

    msg.push_str(&format!(
        "K={} (select {} of {} outcomes)\n\n",
        opp.k, opp.k, opp.m
    ));

    // Investment details
    msg.push_str(&format!("NO Cost: ${:.4}\n", opp.no_cost));

    msg.push_str(&format!("USDC Return: ${:.4}\n", opp.usdc_return));

    if opp.yes_revenue > Decimal::ZERO {
        msg.push_str(&format!("YES Revenue: ${:.4}\n", opp.yes_revenue));
    }

    msg.push_str(&format!("Gas: ${:.4}\n\n", opp.gas_cost));

    // Selected outcomes (NO to buy)
    msg.push_str("<b>Buy NO:</b>\n");
    for (i, sel) in opp.selected_outcomes.iter().take(5).enumerate() {
        let short_q: String = sel.question.chars().take(35).collect();
        msg.push_str(&format!(
            "{}. {} @ ${:.3}\n",
            i + 1,
            html_escape(&short_q),
            sel.no_price
        ));
    }
    if opp.selected_outcomes.len() > 5 {
        msg.push_str(&format!("... +{} more\n", opp.selected_outcomes.len() - 5));
    }

    // Moonbags
    if opp.moonbag_count > 0 {
        msg.push_str(&format!("\n<b>Moonbags: {}</b>\n", opp.moonbag_count));
        for name in opp.moonbag_names.iter().take(3) {
            msg.push_str(&format!("- {}\n", html_escape(name)));
        }
        if opp.moonbag_names.len() > 3 {
            msg.push_str(&format!("... +{} more\n", opp.moonbag_names.len() - 3));
        }
    }

    // Link
    msg.push_str(&format!(
        "\n<a href=\"https://polymarket.com/event/{}\">View on Polymarket</a>",
        opp.market_slug
    ));

    msg
}

/// Escape HTML special characters
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
