//! Slack notification utilities.

use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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

/// Information about a chain's collator slot status
#[derive(Debug, Clone)]
pub struct ChainSlotInfo {
    pub chain_name: String,
    pub is_invulnerable: bool,
    pub is_candidate: bool,
    pub position: Option<usize>,      // Position in candidate list (1-indexed)
    pub max_candidates: Option<u32>,  // Max permissionless slots
    pub total_candidates: usize,      // Total candidates
    pub your_bond: Option<u128>,      // Your current bond
    pub lowest_bond: Option<u128>,    // Lowest candidate bond
    pub distance_from_last: Option<u128>, // How much more than last candidate
    pub last_block_time: Option<std::time::Duration>, // Time since last authored block
    pub token_symbol: String,
    pub decimals: u32,
}

/// Slack notifier for sending alerts
pub struct SlackNotifier {
    webhook_url: Option<String>,
    user_ids: Vec<String>,
    client: reqwest::Client,
    /// Track last notification time per chain for rate limiting
    /// Key: "chain_name:notification_type"
    last_notification: Mutex<HashMap<String, Instant>>,
    /// Track chains with outstanding issues
    outstanding_issues: Mutex<HashSet<String>>,
    /// Track chains that had manual action required (for detecting manual resolution)
    manual_action_chains: Mutex<HashSet<String>>,
}

impl SlackNotifier {
    /// Create a new Slack notifier
    pub fn new(webhook_url: Option<String>, user_ids: Vec<String>) -> Self {
        Self {
            webhook_url,
            user_ids,
            client: reqwest::Client::new(),
            last_notification: Mutex::new(HashMap::new()),
            outstanding_issues: Mutex::new(HashSet::new()),
            manual_action_chains: Mutex::new(HashSet::new()),
        }
    }

    /// Format user mentions for Slack
    fn format_user_mentions(&self) -> String {
        if self.user_ids.is_empty() {
            String::new()
        } else {
            let mentions: Vec<String> = self.user_ids.iter().map(|id| format!("<@{}>", id)).collect();
            format!("\n\ncc: {}", mentions.join(" "))
        }
    }

    /// Add a chain to outstanding issues
    pub fn add_outstanding_issue(&self, chain_name: &str) {
        let mut issues = self.outstanding_issues.lock().unwrap();
        issues.insert(chain_name.to_string());
    }

    /// Remove a chain from outstanding issues
    pub fn remove_outstanding_issue(&self, chain_name: &str) {
        let mut issues = self.outstanding_issues.lock().unwrap();
        issues.remove(chain_name);
    }

    /// Check if chain has outstanding issue
    pub fn has_outstanding_issue(&self, chain_name: &str) -> bool {
        let issues = self.outstanding_issues.lock().unwrap();
        issues.contains(chain_name)
    }

    /// Get list of outstanding issues
    pub fn get_outstanding_issues(&self) -> Vec<String> {
        let issues = self.outstanding_issues.lock().unwrap();
        let mut list: Vec<String> = issues.iter().cloned().collect();
        list.sort();
        list
    }

    /// Mark a chain as requiring manual action
    pub fn mark_manual_action_required(&self, chain_name: &str) {
        let mut manual = self.manual_action_chains.lock().unwrap();
        manual.insert(chain_name.to_string());
    }

    /// Check if chain was marked for manual action (and clear it)
    pub fn was_manual_action_required(&self, chain_name: &str) -> bool {
        let mut manual = self.manual_action_chains.lock().unwrap();
        manual.remove(chain_name)
    }

    /// Format outstanding issues summary
    fn format_outstanding_summary(&self) -> String {
        let issues = self.get_outstanding_issues();
        if issues.is_empty() {
            "\n\n✨ *All chains are now healthy!*".to_string()
        } else {
            format!(
                "\n\n⏳ *Outstanding issues ({}):*\n{}",
                issues.len(),
                issues.iter().map(|c| format!("• {}", c)).collect::<Vec<_>>().join("\n")
            )
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

    /// Send a notification and return the message timestamp (for later deletion)
    /// Note: This only works with bot tokens, not webhooks
    pub async fn send_and_get_ts(&self, message: &str) -> Option<String> {
        // For webhook-based notifications, we can't get the timestamp
        // Log but return None
        let Some(webhook_url) = &self.webhook_url else {
            info!("Slack webhook not configured, skipping notification");
            return None;
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

        match self.client.post(webhook_url).json(&payload).send().await {
            Ok(response) if response.status().is_success() => {
                info!("Slack notification sent (no ts available with webhook)");
                // Webhooks don't return message timestamps
                None
            }
            Ok(response) => {
                warn!("Slack notification failed: {}", response.status());
                None
            }
            Err(e) => {
                warn!("Slack notification error: {}", e);
                None
            }
        }
    }

    /// Delete a message by timestamp (only works with bot tokens, not webhooks)
    pub async fn delete_message(&self, _ts: &str) {
        // Webhook-based notifications can't delete messages
        // This would require a bot token and the chat.delete API
        info!("Message deletion not supported with webhook (would need bot token)");
    }

    /// Send an alert (bypasses rate limiting)
    pub async fn send_alert(&self, message: &str) -> Result<()> {
        let mentions = self.format_user_mentions();
        let full_message = format!("{}{}", message, mentions);
        self.send(&full_message).await
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
        self.add_outstanding_issue(chain_name);

        let rate_key = format!("{}:insufficient_funds", chain_name);
        if self.should_rate_limit(&rate_key) {
            info!("Rate limiting insufficient funds alert for {}", chain_name);
            return Ok(());
        }

        let available = format_balance(available_balance, decimals);
        let required = format_balance(required_balance, decimals);
        let mentions = self.format_user_mentions();

        let message = format!(
            "⚠️ *Collator Registration Alert*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\n\
            Unable to register as collator candidate - insufficient funds.\n\
            • Available: {} {}\n\
            • Minimum required: {} {}\n\n\
            Please top up the account to enable automatic re-registration.\n\n\
            _This alert is rate-limited to once every 4 hours._{}",
            chain_name, collator_address, available, token_symbol, required, token_symbol, mentions
        );

        self.send(&message).await
    }

    /// Send a success notification for registration (always sent immediately, clears rate limits)
    pub async fn notify_registration_success(
        &self,
        chain_name: &str,
        collator_address: &str,
        bond_amount: u128,
        token_symbol: &str,
        decimals: u32,
    ) -> Result<()> {
        // Clear rate limits and remove from outstanding issues
        self.clear_rate_limit(chain_name);
        self.remove_outstanding_issue(chain_name);

        let bond = format_balance(bond_amount, decimals);
        let outstanding = self.format_outstanding_summary();

        let message = format!(
            "✅ *Collator Registration Success*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\
            *Bond Amount:* {} {}\n\n\
            Successfully registered as collator candidate via proxy.{}",
            chain_name, collator_address, bond, token_symbol, outstanding
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
        // Clear rate limits and remove from outstanding issues
        self.clear_rate_limit(chain_name);
        self.remove_outstanding_issue(chain_name);

        let old = format_balance(old_bond, decimals);
        let new = format_balance(new_bond, decimals);
        let outstanding = self.format_outstanding_summary();

        let message = format!(
            "📈 *Collator Bond Updated*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\
            *Previous Bond:* {} {}\n\
            *New Bond:* {} {}\n\n\
            Bond increased via proxy to maximize competitiveness.{}",
            chain_name, collator_address, old, token_symbol, new, token_symbol, outstanding
        );

        self.send(&message).await
    }

    /// Notify that a chain issue was resolved (detected healthy after being in outstanding)
    pub async fn notify_issue_resolved(
        &self,
        chain_name: &str,
        was_manual: bool,
        current_status: &str,
    ) -> Result<()> {
        self.clear_rate_limit(chain_name);
        self.remove_outstanding_issue(chain_name);

        let resolution_type = if was_manual {
            "Manual intervention"
        } else {
            "Automatic action"
        };
        let outstanding = self.format_outstanding_summary();

        let message = format!(
            "✅ *Issue Resolved*\n\n\
            *Chain:* {}\n\
            *Resolution:* {}\n\
            *Current Status:* {}\n\n\
            Chain is now healthy.{}",
            chain_name, resolution_type, current_status, outstanding
        );

        self.send(&message).await
    }

    /// Send an error notification (rate limited)
    pub async fn notify_error(&self, chain_name: &str, error_message: &str) -> Result<()> {
        self.add_outstanding_issue(chain_name);

        let rate_key = format!("{}:error", chain_name);
        if self.should_rate_limit(&rate_key) {
            info!("Rate limiting error alert for {}", chain_name);
            return Ok(());
        }

        let mentions = self.format_user_mentions();

        let message = format!(
            "❌ *Collator Monitor Error*\n\n\
            *Chain:* {}\n\
            *Error:* {}\n\n\
            Please investigate and take manual action if needed.\n\n\
            _This alert is rate-limited to once every 4 hours._{}",
            chain_name, error_message, mentions
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
        self.add_outstanding_issue(chain_name);

        let rate_key = format!("{}:cannot_compete", chain_name);
        if self.should_rate_limit(&rate_key) {
            info!("Rate limiting cannot compete alert for {}", chain_name);
            return Ok(());
        }

        let available = format_balance(available_balance, decimals);
        let lowest = format_balance(lowest_candidate_bond, decimals);
        let need_more = format_balance(needed, decimals);
        let mentions = self.format_user_mentions();

        let message = format!(
            "⚠️ *Cannot Compete for Collator Slot*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\n\
            Unable to register - bond too low to beat existing candidates.\n\
            • Your available bond: {} {}\n\
            • Lowest candidate bond: {} {}\n\
            • Need additional: {} {}\n\n\
            Please top up the account to compete for a collator slot.\n\n\
            _This alert is rate-limited to once every 4 hours._{}",
            chain_name, collator_address, 
            available, token_symbol, 
            lowest, token_symbol,
            need_more, token_symbol,
            mentions
        );

        self.send(&message).await
    }

    /// Send an alert when manual action is required (rate limited)
    pub async fn alert_manual_action_required(
        &self,
        chain_name: &str,
        collator_address: &str,
        action_required: &str,
        batch_call_data: Option<&str>,
    ) -> Result<()> {
        self.add_outstanding_issue(chain_name);
        self.mark_manual_action_required(chain_name);

        let rate_key = format!("{}:manual_action", chain_name);
        if self.should_rate_limit(&rate_key) {
            info!("Rate limiting manual action alert for {}", chain_name);
            return Ok(());
        }

        let mentions = self.format_user_mentions();
        
        let call_data_section = if let Some(data) = batch_call_data {
            format!("\n\n*Batch Call Data (for Polkadot.js Developer > Extrinsics > Decode):*\n```{}```", data)
        } else {
            String::new()
        };

        let message = format!(
            "🔧 *Manual Action Required*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\n\
            Automatic action not possible on this chain.\n\
            *Action needed:* {}{}\n\n\
            Please perform this action manually via Polkadot.js or similar.\n\n\
            _This alert is rate-limited to once every 4 hours._{}",
            chain_name, collator_address, action_required, call_data_section, mentions
        );

        self.send(&message).await
    }

    /// Send a periodic summary of all collator slot statuses
    pub async fn send_status_summary(&self, slots: &[ChainSlotInfo]) -> Result<()> {
        let mut lines = vec!["📊 *Collator Slot Status Summary*\n".to_string()];

        for slot in slots {
            let status = if slot.is_invulnerable {
                "🛡️ *Safe* (Invulnerable)".to_string()
            } else if slot.is_candidate {
                let position = slot.position.unwrap_or(0);
                let max = slot.max_candidates.unwrap_or(0);
                let total = slot.total_candidates;
                
                let position_str = if max > 0 {
                    format!("#{} of {} (max {})", position, total, max)
                } else {
                    format!("#{} of {}", position, total)
                };
                
                let distance_str = if let Some(dist) = slot.distance_from_last {
                    let formatted = format_balance(dist, slot.decimals);
                    format!(" | +{} {} from last", formatted, slot.token_symbol)
                } else {
                    String::new()
                };
                
                // Check if position is outside the active set (position > max)
                let is_outside_active = max > 0 && position > max as usize;
                
                if is_outside_active {
                    // Warning - outside active set, needs more bond
                    format!("⚠️ {}{} *OUTSIDE ACTIVE SET*", position_str, distance_str)
                } else {
                    // Healthy - in active set
                    format!("✅ {}{}", position_str, distance_str)
                }
            } else {
                "❌ *Not a collator*".to_string()
            };

            // Add last block time (only for collators)
            let last_block_str = if slot.is_invulnerable || slot.is_candidate {
                match slot.last_block_time {
                    Some(duration) => format!(" | Last block: {}", format_duration(duration)),
                    None => " | Last block: unknown".to_string(),
                }
            } else {
                String::new()
            };

            lines.push(format!("• *{}:* {}{}", slot.chain_name, status, last_block_str));
        }

        // Add outstanding issues if any
        let issues = self.get_outstanding_issues();
        if !issues.is_empty() {
            lines.push(format!(
                "\n⏳ *Outstanding issues ({}):*\n{}",
                issues.len(),
                issues.iter().map(|c| format!("• {}", c)).collect::<Vec<_>>().join("\n")
            ));
        }

        let message = lines.join("\n");
        self.send(&message).await
    }
}

/// Format a duration in human-readable form
fn format_duration(duration: std::time::Duration) -> String {
    let total_secs = duration.as_secs();
    
    if total_secs < 60 {
        format!("{}s ago", total_secs)
    } else if total_secs < 3600 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        if secs == 0 {
            format!("{}min ago", mins)
        } else {
            format!("{}min {}s ago", mins, secs)
        }
    } else if total_secs < 86400 {
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        if mins == 0 {
            format!("{}hr ago", hours)
        } else {
            format!("{}hr {}min ago", hours, mins)
        }
    } else {
        let days = total_secs / 86400;
        let hours = (total_secs % 86400) / 3600;
        if hours == 0 {
            if days == 1 {
                "1 day ago".to_string()
            } else {
                format!("{} days ago", days)
            }
        } else {
            if days == 1 {
                format!("1 day {}hr ago", hours)
            } else {
                format!("{} days {}hr ago", days, hours)
            }
        }
    }
}

/// Format a balance with proper decimal places
pub fn format_balance(balance: u128, decimals: u32) -> String {
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
