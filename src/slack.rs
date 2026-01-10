//! Slack notification utilities.

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Slack message payload
#[derive(Serialize)]
struct SlackMessage {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<Vec<SlackBlock>>,
}

#[derive(Serialize)]
struct SlackBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<SlackText>,
}

#[derive(Serialize)]
struct SlackText {
    #[serde(rename = "type")]
    text_type: String,
    text: String,
}

/// Rate limit configuration
const RATE_LIMIT_DURATION: Duration = Duration::from_secs(4 * 60 * 60); // 4 hours

/// Slack notifier for sending alerts
pub struct SlackNotifier {
    webhook_url: Option<String>,
    client: reqwest::Client,
    /// Track last notification time per chain for rate limiting
    /// Key: "chain_name:notification_type"
    last_notification: Mutex<HashMap<String, Instant>>,
}

impl SlackNotifier {
    /// Create a new Slack notifier
    pub fn new(webhook_url: Option<String>) -> Self {
        Self {
            webhook_url,
            client: reqwest::Client::new(),
            last_notification: Mutex::new(HashMap::new()),
        }
    }

    /// Check if we should rate limit this notification
    fn should_rate_limit(&self, key: &str) -> bool {
        let mut last = self.last_notification.lock().unwrap();
        if let Some(last_time) = last.get(key) {
            if last_time.elapsed() < RATE_LIMIT_DURATION {
                return true;
            }
        }
        // Update the timestamp
        last.insert(key.to_string(), Instant::now());
        false
    }

    /// Clear rate limit for a key (called on success to allow immediate future alerts)
    fn clear_rate_limit(&self, chain_name: &str) {
        let mut last = self.last_notification.lock().unwrap();
        // Clear all rate limits for this chain
        last.retain(|k, _| !k.starts_with(chain_name));
    }

    /// Send a notification to Slack
    async fn send(&self, message: &str) -> Result<()> {
        let Some(webhook_url) = &self.webhook_url else {
            info!("Slack webhook not configured, skipping notification");
            info!("Message would have been: {}", message);
            return Ok(());
        };

        let payload = SlackMessage {
            text: message.to_string(),
            blocks: Some(vec![SlackBlock {
                block_type: "section".to_string(),
                text: Some(SlackText {
                    text_type: "mrkdwn".to_string(),
                    text: message.to_string(),
                }),
            }]),
        };

        let response = self
            .client
            .post(webhook_url)
            .json(&payload)
            .send()
            .await?;

        if response.status().is_success() {
            info!("Slack notification sent successfully");
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!("Failed to send Slack notification: {} - {}", status, body);
            Err(anyhow::anyhow!(
                "Slack notification failed: {} - {}",
                status,
                body
            ))
        }
    }

    /// Send a notification (rate limited for non-success messages)
    pub async fn notify(&self, message: &str) -> Result<()> {
        self.send(message).await
    }

    /// Send an alert about insufficient funds (rate limited)
    pub async fn alert_insufficient_funds(
        &self,
        chain_name: &str,
        collator_address: &str,
        available_balance: u128,
        required_balance: u128,
        token_symbol: &str,
        decimals: u32,
    ) -> Result<()> {
        let rate_key = format!("{}:insufficient_funds", chain_name);
        if self.should_rate_limit(&rate_key) {
            info!("Rate limiting insufficient funds alert for {}", chain_name);
            return Ok(());
        }

        let available = format_balance(available_balance, decimals);
        let required = format_balance(required_balance, decimals);

        let message = format!(
            "⚠️ *Collator Registration Alert*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\n\
            Unable to register as collator candidate - insufficient funds.\n\
            • Available: {} {}\n\
            • Minimum required: {} {}\n\n\
            Please top up the account to enable automatic re-registration.\n\n\
            _This alert is rate-limited to once every 4 hours._",
            chain_name, collator_address, available, token_symbol, required, token_symbol
        );

        self.send(&message).await
    }

    /// Send a success notification (always sent immediately, clears rate limits)
    pub async fn notify_registration_success(
        &self,
        chain_name: &str,
        collator_address: &str,
        bond_amount: u128,
        token_symbol: &str,
        decimals: u32,
    ) -> Result<()> {
        // Clear rate limits for this chain since issue is resolved
        self.clear_rate_limit(chain_name);

        let bond = format_balance(bond_amount, decimals);

        let message = format!(
            "✅ *Collator Registration Success*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\
            *Bond Amount:* {} {}\n\n\
            Successfully registered as collator candidate.",
            chain_name, collator_address, bond, token_symbol
        );

        self.send(&message).await
    }

    /// Send a bond update notification (always sent immediately, clears rate limits)
    pub async fn notify_bond_update(
        &self,
        chain_name: &str,
        collator_address: &str,
        old_bond: u128,
        new_bond: u128,
        token_symbol: &str,
        decimals: u32,
    ) -> Result<()> {
        // Clear rate limits for this chain since we successfully took action
        self.clear_rate_limit(chain_name);

        let old = format_balance(old_bond, decimals);
        let new = format_balance(new_bond, decimals);

        let message = format!(
            "📈 *Collator Bond Updated*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\
            *Previous Bond:* {} {}\n\
            *New Bond:* {} {}\n\n\
            Bond increased to maximize competitiveness.",
            chain_name, collator_address, old, token_symbol, new, token_symbol
        );

        self.send(&message).await
    }

    /// Send an error notification (rate limited)
    pub async fn notify_error(&self, chain_name: &str, error_message: &str) -> Result<()> {
        let rate_key = format!("{}:error", chain_name);
        if self.should_rate_limit(&rate_key) {
            info!("Rate limiting error alert for {}", chain_name);
            return Ok(());
        }

        let message = format!(
            "❌ *Collator Monitor Error*\n\n\
            *Chain:* {}\n\
            *Error:* {}\n\n\
            Please investigate and take manual action if needed.\n\n\
            _This alert is rate-limited to once every 4 hours._",
            chain_name, error_message
        );

        self.send(&message).await
    }

    /// Send an alert about being unable to compete with existing candidates (rate limited)
    pub async fn alert_cannot_compete(
        &self,
        chain_name: &str,
        collator_address: &str,
        available_balance: u128,
        lowest_candidate_bond: u128,
        needed: u128,
        token_symbol: &str,
        decimals: u32,
    ) -> Result<()> {
        let rate_key = format!("{}:cannot_compete", chain_name);
        if self.should_rate_limit(&rate_key) {
            info!("Rate limiting cannot compete alert for {}", chain_name);
            return Ok(());
        }

        let available = format_balance(available_balance, decimals);
        let lowest = format_balance(lowest_candidate_bond, decimals);
        let need_more = format_balance(needed, decimals);

        let message = format!(
            "⚠️ *Cannot Compete for Collator Slot*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\n\
            Unable to register - bond too low to beat existing candidates.\n\
            • Your available bond: {} {}\n\
            • Lowest candidate bond: {} {}\n\
            • Need additional: {} {}\n\n\
            Please top up the account to compete for a collator slot.\n\n\
            _This alert is rate-limited to once every 4 hours._",
            chain_name, collator_address, 
            available, token_symbol, 
            lowest, token_symbol,
            need_more, token_symbol
        );

        self.send(&message).await
    }

    /// Send an alert when manual action is required (rate limited)
    pub async fn alert_manual_action_required(
        &self,
        chain_name: &str,
        collator_address: &str,
        action_required: &str,
    ) -> Result<()> {
        let rate_key = format!("{}:manual_action", chain_name);
        if self.should_rate_limit(&rate_key) {
            info!("Rate limiting manual action alert for {}", chain_name);
            return Ok(());
        }

        let message = format!(
            "🔧 *Manual Action Required*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\n\
            Automatic action not possible on this chain.\n\
            *Action needed:* {}\n\n\
            Please perform this action manually via Polkadot.js or similar.\n\n\
            _This alert is rate-limited to once every 4 hours._",
            chain_name, collator_address, action_required
        );

        self.send(&message).await
    }
}

/// Format a balance with proper decimal places
fn format_balance(balance: u128, decimals: u32) -> String {
    let divisor = 10u128.pow(decimals);
    let whole = balance / divisor;
    let fraction = balance % divisor;

    if fraction == 0 {
        format!("{}", whole)
    } else {
        // Format with up to 4 decimal places
        let fraction_str = format!("{:0>width$}", fraction, width = decimals as usize);
        let trimmed = fraction_str.trim_end_matches('0');
        let display_decimals = trimmed.len().min(4);
        format!("{}.{}", whole, &fraction_str[..display_decimals])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_balance() {
        // DOT (10 decimals)
        assert_eq!(format_balance(10_000_000_000, 10), "1");
        assert_eq!(format_balance(15_000_000_000, 10), "1.5");
        assert_eq!(format_balance(12_345_000_000, 10), "1.2345");

        // KSM (12 decimals)
        assert_eq!(format_balance(1_000_000_000_000, 12), "1");
        assert_eq!(format_balance(100_000_000_000, 12), "0.1");
    }
}
