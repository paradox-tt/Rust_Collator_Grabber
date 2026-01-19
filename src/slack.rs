//! Slack notification utilities with support for message updates and deletions.
//!
//! Supports both webhook URLs (limited) and bot tokens (full features).
//! Bot tokens enable updating and deleting messages.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn, debug};

/// Slack message payload for posting
#[derive(Serialize)]
struct SlackPostMessage {
    channel: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<Vec<SlackBlock>>,
}

/// Slack message payload for updating
#[derive(Serialize)]
struct SlackUpdateMessage {
    channel: String,
    ts: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<Vec<SlackBlock>>,
}

/// Slack message payload for deleting
#[derive(Serialize)]
struct SlackDeleteMessage {
    channel: String,
    ts: String,
}

/// Slack webhook payload (simpler format)
#[derive(Serialize)]
struct SlackWebhookMessage {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<Vec<SlackBlock>>,
}

#[derive(Serialize, Clone)]
struct SlackBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<SlackText>,
}

#[derive(Serialize, Clone)]
struct SlackText {
    #[serde(rename = "type")]
    text_type: String,
    text: String,
}

/// Response from Slack API
#[derive(Deserialize)]
struct SlackResponse {
    ok: bool,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Rate limit configuration
const RATE_LIMIT_DURATION: Duration = Duration::from_secs(4 * 60 * 60); // 4 hours

/// Information about a chain's collator slot status
#[derive(Debug, Clone)]
pub struct ChainSlotInfo {
    pub chain_name: String,
    pub is_invulnerable: bool,
    pub is_candidate: bool,
    pub position: Option<usize>,
    pub max_candidates: Option<u32>,
    pub total_candidates: usize,
    pub your_bond: Option<u128>,
    pub lowest_bond: Option<u128>,
    pub distance_from_last: Option<u128>,
    pub last_block_time: Option<std::time::Duration>,
    pub token_symbol: String,
    pub decimals: u32,
}

/// Reference to a posted Slack message (for updates/deletes)
#[derive(Debug, Clone)]
pub struct MessageRef {
    pub channel: String,
    pub ts: String,
    pub posted_at: Instant,
}

/// Tracked alert state
#[derive(Debug)]
struct TrackedAlert {
    message_ref: MessageRef,
    started_at: Instant,
}

/// Helper for async delete operations (to avoid cloning full SlackNotifier)
#[derive(Clone)]
struct DeleteHelper {
    bot_token: Option<String>,
    client: reqwest::Client,
}

impl DeleteHelper {
    async fn delete_message_by_ref(&self, msg_ref: &MessageRef) {
        let Some(token) = &self.bot_token else {
            return;
        };

        if msg_ref.ts.is_empty() {
            return;
        }

        let payload = serde_json::json!({
            "channel": msg_ref.channel,
            "ts": msg_ref.ts,
        });

        let _ = self.client
            .post("https://slack.com/api/chat.delete")
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await;
    }
}

/// Slack notifier for sending alerts
pub struct SlackNotifier {
    /// Webhook URL (for simple posting only)
    webhook_url: Option<String>,
    /// Bot token (for full API access - post, update, delete)
    bot_token: Option<String>,
    /// Channel ID or name (required for bot token)
    channel: Option<String>,
    /// User IDs to mention for ON-CHAIN actions (registration, bond updates, manual actions)
    user_ids_onchain: Vec<String>,
    /// User IDs to mention for OPS issues (block production, disconnections)
    user_ids_ops: Vec<String>,
    /// HTTP client
    client: reqwest::Client,
    /// Track last notification time per chain for rate limiting
    last_notification: Mutex<HashMap<String, Instant>>,
    /// Track chains with outstanding issues
    outstanding_issues: Mutex<HashSet<String>>,
    /// Track chains that had manual action required
    manual_action_chains: Mutex<HashSet<String>>,
    /// Track disconnect alerts by chain name
    disconnect_alerts: Mutex<HashMap<String, TrackedAlert>>,
    /// Track block production alerts by chain name
    block_alerts: Mutex<HashMap<String, TrackedAlert>>,
}

impl SlackNotifier {
    /// Create a new Slack notifier
    /// 
    /// For full functionality (update/delete messages), provide bot_token and channel.
    /// Webhook URL can still be used for simple notifications.
    pub fn new(webhook_url: Option<String>, user_ids_onchain: Vec<String>, user_ids_ops: Vec<String>) -> Self {
        Self {
            webhook_url,
            bot_token: None,
            channel: None,
            user_ids_onchain,
            user_ids_ops,
            client: reqwest::Client::new(),
            last_notification: Mutex::new(HashMap::new()),
            outstanding_issues: Mutex::new(HashSet::new()),
            manual_action_chains: Mutex::new(HashSet::new()),
            disconnect_alerts: Mutex::new(HashMap::new()),
            block_alerts: Mutex::new(HashMap::new()),
        }
    }

    /// Create with bot token for full API access
    pub fn with_bot_token(bot_token: String, channel: String, user_ids_onchain: Vec<String>, user_ids_ops: Vec<String>) -> Self {
        Self {
            webhook_url: None,
            bot_token: Some(bot_token),
            channel: Some(channel),
            user_ids_onchain,
            user_ids_ops,
            client: reqwest::Client::new(),
            last_notification: Mutex::new(HashMap::new()),
            outstanding_issues: Mutex::new(HashSet::new()),
            manual_action_chains: Mutex::new(HashSet::new()),
            disconnect_alerts: Mutex::new(HashMap::new()),
            block_alerts: Mutex::new(HashMap::new()),
        }
    }

    /// Check if we have bot token capability
    fn has_bot_token(&self) -> bool {
        self.bot_token.is_some() && self.channel.is_some()
    }

    /// Create a lightweight clone for async delete operations
    fn clone_for_delete(&self) -> DeleteHelper {
        DeleteHelper {
            bot_token: self.bot_token.clone(),
            client: self.client.clone(),
        }
    }

    /// Format user mentions for on-chain action alerts
    fn format_onchain_mentions(&self) -> String {
        if self.user_ids_onchain.is_empty() {
            String::new()
        } else {
            let mentions: Vec<String> = self.user_ids_onchain.iter().map(|id| format!("<@{}>", id)).collect();
            format!("\n\ncc: {}", mentions.join(" "))
        }
    }

    /// Format user mentions for ops/infrastructure alerts
    fn format_ops_mentions(&self) -> String {
        if self.user_ids_ops.is_empty() {
            String::new()
        } else {
            let mentions: Vec<String> = self.user_ids_ops.iter().map(|id| format!("<@{}>", id)).collect();
            format!("\n\ncc: {}", mentions.join(" "))
        }
    }

    /// Format duration for display
    fn format_duration(d: Duration) -> String {
        let secs = d.as_secs();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            let hours = secs / 3600;
            let mins = (secs % 3600) / 60;
            format!("{}h {}m", hours, mins)
        }
    }

    // ==================== Connection Alert Methods ====================

    /// Report a disconnection - posts new message or updates existing
    pub async fn report_disconnect(&self, chain_name: &str, error: &str) {
        let now = Instant::now();
        
        // Check if we already have an alert for this chain
        let existing = {
            let alerts = self.disconnect_alerts.lock().unwrap();
            alerts.get(chain_name).map(|a| (a.message_ref.clone(), a.started_at))
        };

        if let Some((msg_ref, started_at)) = existing {
            // Update existing message with duration (only if we have bot token)
            let duration = Self::format_duration(now.duration_since(started_at));
            let message = format!(
                "⚠️ *{}* disconnected for *{}*\nError: `{}`\nReconnecting...",
                chain_name, duration, error
            );
            
            if let Err(e) = self.update_message(&msg_ref, &message).await {
                debug!("Failed to update disconnect message: {}", e);
            }
        } else {
            // Post new message
            let message = format!(
                "⚠️ *{}* disconnected\nError: `{}`\nReconnecting...",
                chain_name, error
            );
            
            // Track the alert regardless of whether we get a message ref back
            let msg_ref = self.post_message(&message).await.unwrap_or_else(|| {
                // Create a dummy ref for tracking purposes (no bot token case)
                MessageRef {
                    channel: String::new(),
                    ts: String::new(),
                    posted_at: now,
                }
            });
            
            let mut alerts = self.disconnect_alerts.lock().unwrap();
            alerts.insert(chain_name.to_string(), TrackedAlert {
                message_ref: msg_ref,
                started_at: now,
            });
        }
    }

    /// Report reconnection - updates message to show restored, then deletes after delay
    pub async fn report_reconnect(&self, chain_name: &str) {
        let alert = {
            let mut alerts = self.disconnect_alerts.lock().unwrap();
            alerts.remove(chain_name)
        };

        if let Some(alert) = alert {
            let duration = Self::format_duration(alert.started_at.elapsed());
            
            // Update message to show reconnected
            let message = format!(
                "✅ *{}* reconnected after *{}*",
                chain_name, duration
            );
            
            // Try to update the message (will work with bot token)
            if !alert.message_ref.ts.is_empty() {
                let _ = self.update_message(&alert.message_ref, &message).await;
                
                // Delete after a short delay so user can see the "reconnected" message
                let msg_ref = alert.message_ref.clone();
                let client = self.clone_for_delete();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    client.delete_message_by_ref(&msg_ref).await;
                });
            }
            
            info!("{}: Reconnected after {}", chain_name, duration);
        }
    }

    // ==================== Block Production Alert Methods ====================

    /// Report no blocks authored - posts new message or updates existing
    pub async fn report_no_blocks(&self, chain_name: &str, duration_since_last: Duration) {
        let now = Instant::now();
        let duration_str = Self::format_duration(duration_since_last);
        
        // Check if we already have an alert for this chain
        let existing = {
            let alerts = self.block_alerts.lock().unwrap();
            alerts.get(chain_name).map(|a| (a.message_ref.clone(), a.started_at))
        };

        let message = format!(
            "⏰ *{}* - No blocks authored in *{}*\nPlease check collator status.{}",
            chain_name, duration_str, self.format_ops_mentions()
        );

        if let Some((msg_ref, _)) = existing {
            // Update existing message
            if !msg_ref.ts.is_empty() {
                if let Err(e) = self.update_message(&msg_ref, &message).await {
                    debug!("Failed to update block alert message: {}", e);
                }
            }
        } else {
            // Post new message and track it
            let msg_ref = self.post_message(&message).await.unwrap_or_else(|| {
                // Create a dummy ref for tracking purposes
                MessageRef {
                    channel: String::new(),
                    ts: String::new(),
                    posted_at: now,
                }
            });
            
            let mut alerts = self.block_alerts.lock().unwrap();
            alerts.insert(chain_name.to_string(), TrackedAlert {
                message_ref: msg_ref,
                started_at: now,
            });
        }
    }

    /// Report block authored - updates message to show restored, then deletes
    pub async fn report_block_authored(&self, chain_name: &str) {
        let alert = {
            let mut alerts = self.block_alerts.lock().unwrap();
            alerts.remove(chain_name)
        };

        if let Some(alert) = alert {
            let duration = Self::format_duration(alert.started_at.elapsed());
            
            // Update message to show block production restored
            let message = format!(
                "✅ *{}* - Block production restored after *{}*",
                chain_name, duration
            );
            
            // Try to update the message (will work with bot token)
            if !alert.message_ref.ts.is_empty() {
                let _ = self.update_message(&alert.message_ref, &message).await;
                
                // Delete after a short delay so user can see the "restored" message
                let msg_ref = alert.message_ref.clone();
                let client = self.clone_for_delete();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    client.delete_message_by_ref(&msg_ref).await;
                });
            }
            
            info!("{}: Block production restored after {}", chain_name, duration);
        }
    }

    // ==================== Core Message Methods ====================

    /// Post a message and return reference for later update/delete
    async fn post_message(&self, text: &str) -> Option<MessageRef> {
        if let (Some(token), Some(channel)) = (&self.bot_token, &self.channel) {
            // Use bot token API
            let payload = SlackPostMessage {
                channel: channel.clone(),
                text: text.to_string(),
                blocks: Some(vec![SlackBlock {
                    block_type: "section".to_string(),
                    text: Some(SlackText {
                        text_type: "mrkdwn".to_string(),
                        text: text.to_string(),
                    }),
                }]),
            };

            match self.client
                .post("https://slack.com/api/chat.postMessage")
                .bearer_auth(token)
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => {
                    match resp.json::<SlackResponse>().await {
                        Ok(slack_resp) if slack_resp.ok => {
                            if let (Some(ts), Some(ch)) = (slack_resp.ts, slack_resp.channel) {
                                return Some(MessageRef {
                                    channel: ch,
                                    ts,
                                    posted_at: Instant::now(),
                                });
                            }
                        }
                        Ok(slack_resp) => {
                            warn!("Slack API error: {:?}", slack_resp.error);
                        }
                        Err(e) => {
                            warn!("Failed to parse Slack response: {}", e);
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to post Slack message: {}", e);
                }
            }
        } else if let Some(webhook_url) = &self.webhook_url {
            // Fall back to webhook (can't get message ref)
            let payload = SlackWebhookMessage {
                text: text.to_string(),
                blocks: Some(vec![SlackBlock {
                    block_type: "section".to_string(),
                    text: Some(SlackText {
                        text_type: "mrkdwn".to_string(),
                        text: text.to_string(),
                    }),
                }]),
            };

            if let Err(e) = self.client.post(webhook_url).json(&payload).send().await {
                warn!("Failed to send webhook: {}", e);
            }
        }
        
        None
    }

    /// Update an existing message
    async fn update_message(&self, msg_ref: &MessageRef, text: &str) -> Result<()> {
        let Some(token) = &self.bot_token else {
            // Can't update without bot token
            return Ok(());
        };

        let payload = SlackUpdateMessage {
            channel: msg_ref.channel.clone(),
            ts: msg_ref.ts.clone(),
            text: text.to_string(),
            blocks: Some(vec![SlackBlock {
                block_type: "section".to_string(),
                text: Some(SlackText {
                    text_type: "mrkdwn".to_string(),
                    text: text.to_string(),
                }),
            }]),
        };

        let resp = self.client
            .post("https://slack.com/api/chat.update")
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await?;

        let slack_resp: SlackResponse = resp.json().await?;
        if !slack_resp.ok {
            warn!("Failed to update message: {:?}", slack_resp.error);
        }

        Ok(())
    }

    /// Delete a message by reference
    async fn delete_message_by_ref(&self, msg_ref: &MessageRef) {
        let Some(token) = &self.bot_token else {
            return;
        };

        let payload = SlackDeleteMessage {
            channel: msg_ref.channel.clone(),
            ts: msg_ref.ts.clone(),
        };

        match self.client
            .post("https://slack.com/api/chat.delete")
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(slack_resp) = resp.json::<SlackResponse>().await {
                    if !slack_resp.ok {
                        debug!("Failed to delete message: {:?}", slack_resp.error);
                    }
                }
            }
            Err(e) => {
                debug!("Failed to delete message: {}", e);
            }
        }
    }

    // ==================== Legacy Methods (for compatibility) ====================

    /// Send a notification and return the message timestamp (for later deletion)
    pub async fn send_and_get_ts(&self, message: &str) -> Option<String> {
        self.post_message(message).await.map(|r| r.ts)
    }

    /// Delete a message by timestamp
    pub async fn delete_message(&self, ts: &str) {
        if let Some(channel) = &self.channel {
            let msg_ref = MessageRef {
                channel: channel.clone(),
                ts: ts.to_string(),
                posted_at: Instant::now(),
            };
            self.delete_message_by_ref(&msg_ref).await;
        }
    }

    /// Send an alert (bypasses rate limiting) - for ops/status alerts
    pub async fn send_alert(&self, message: &str) -> Result<()> {
        let mentions = self.format_ops_mentions();
        let full_message = format!("{}{}", message, mentions);
        self.send(&full_message).await
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

    /// Check if a chain has an outstanding issue
    pub fn has_outstanding_issue(&self, chain_name: &str) -> bool {
        let issues = self.outstanding_issues.lock().unwrap();
        issues.contains(chain_name)
    }

    /// Mark a chain as having manual action required
    pub fn mark_manual_action_required(&self, chain_name: &str) {
        let mut chains = self.manual_action_chains.lock().unwrap();
        chains.insert(chain_name.to_string());
    }

    /// Check if manual action was required (for detecting resolution)
    pub fn was_manual_action_required(&self, chain_name: &str) -> bool {
        let chains = self.manual_action_chains.lock().unwrap();
        chains.contains(chain_name)
    }

    /// Clear manual action required status
    pub fn clear_manual_action_required(&self, chain_name: &str) {
        let mut chains = self.manual_action_chains.lock().unwrap();
        chains.remove(chain_name);
    }

    /// Check if we should send a notification (rate limiting)
    fn should_notify(&self, key: &str) -> bool {
        let mut last = self.last_notification.lock().unwrap();
        if let Some(last_time) = last.get(key) {
            if last_time.elapsed() < RATE_LIMIT_DURATION {
                return false;
            }
        }
        last.insert(key.to_string(), Instant::now());
        true
    }

    /// Send a notification to Slack
    async fn send(&self, message: &str) -> Result<()> {
        // Try bot token first
        if let (Some(token), Some(channel)) = (&self.bot_token, &self.channel) {
            let payload = SlackPostMessage {
                channel: channel.clone(),
                text: message.to_string(),
                blocks: Some(vec![SlackBlock {
                    block_type: "section".to_string(),
                    text: Some(SlackText {
                        text_type: "mrkdwn".to_string(),
                        text: message.to_string(),
                    }),
                }]),
            };

            let response = self.client
                .post("https://slack.com/api/chat.postMessage")
                .bearer_auth(token)
                .json(&payload)
                .send()
                .await?;

            if response.status().is_success() {
                info!("Slack notification sent successfully");
                return Ok(());
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!("Failed to send Slack notification: {} - {}", status, body);
            }
        }
        
        // Fall back to webhook
        if let Some(webhook_url) = &self.webhook_url {
            let payload = SlackWebhookMessage {
                text: message.to_string(),
                blocks: Some(vec![SlackBlock {
                    block_type: "section".to_string(),
                    text: Some(SlackText {
                        text_type: "mrkdwn".to_string(),
                        text: message.to_string(),
                    }),
                }]),
            };

            let response = self.client
                .post(webhook_url)
                .json(&payload)
                .send()
                .await?;

            if response.status().is_success() {
                info!("Slack notification sent successfully (webhook)");
                return Ok(());
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!("Failed to send Slack notification: {} - {}", status, body);
                return Err(anyhow::anyhow!("Slack notification failed: {} - {}", status, body));
            }
        }

        info!("Slack not configured, skipping notification");
        info!("Message would have been: {}", message);
        Ok(())
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
        if !self.should_notify(&rate_key) {
            info!("Rate limited: insufficient funds alert for {}", chain_name);
            return Ok(());
        }

        let available = format_balance(available_balance, decimals, token_symbol);
        let required = format_balance(required_balance, decimals, token_symbol);
        let mentions = self.format_onchain_mentions();

        let message = format!(
            "⚠️ *Insufficient funds* on *{}*\n\n\
            Collator: `{}`\n\
            Available: {}\n\
            Required: {}\n\n\
            Please add funds to continue as a candidate.{}\n\n\
            _This alert is rate-limited to once every 4 hours._",
            chain_name, collator_address, available, required, mentions
        );

        self.send(&message).await
    }

    /// Send an alert requiring manual action (rate limited)
    pub async fn alert_manual_action_required(
        &self,
        chain_name: &str,
        collator_address: &str,
        action_description: &str,
        call_data: Option<&str>,
    ) -> Result<()> {
        self.add_outstanding_issue(chain_name);
        self.mark_manual_action_required(chain_name);

        let rate_key = format!("{}:manual_action", chain_name);
        if !self.should_notify(&rate_key) {
            info!("Rate limited: manual action alert for {}", chain_name);
            return Ok(());
        }

        let mentions = self.format_onchain_mentions();
        
        let call_data_section = if let Some(data) = call_data {
            format!(
                "\n\n*Batch Call Data* (for Polkadot.js Developer > Extrinsics > Decode):\n```{}```",
                data
            )
        } else {
            String::new()
        };

        let message = format!(
            "🔧 *Manual Action Required*\n\n\
            *Chain:* {}\n\
            *Collator:* `{}`\n\n\
            Automatic action not possible on this chain.\n\
            *Action needed:* {}{}\n\n\
            Please perform this action manually via Polkadot.js or similar.{}\n\n\
            _This alert is rate-limited to once every 4 hours._",
            chain_name, collator_address, action_description, call_data_section, mentions
        );

        self.send(&message).await
    }

    /// Notify about successful registration
    pub async fn notify_registration(
        &self,
        chain_name: &str,
        collator_address: &str,
        bond_amount: u128,
        token_symbol: &str,
        decimals: u32,
    ) -> Result<()> {
        self.remove_outstanding_issue(chain_name);
        self.clear_manual_action_required(chain_name);

        let bond = format_balance(bond_amount, decimals, token_symbol);
        let message = format!(
            "✅ *Registered as candidate* on *{}*\n\n\
            Collator: `{}`\n\
            Bond: {}",
            chain_name, collator_address, bond
        );

        self.send(&message).await
    }

    /// Notify about bond update
    pub async fn notify_bond_update(
        &self,
        chain_name: &str,
        collator_address: &str,
        old_bond: u128,
        new_bond: u128,
        token_symbol: &str,
        decimals: u32,
    ) -> Result<()> {
        let old = format_balance(old_bond, decimals, token_symbol);
        let new = format_balance(new_bond, decimals, token_symbol);
        let message = format!(
            "📈 *Bond updated* on *{}*\n\n\
            Collator: `{}`\n\
            Previous: {}\n\
            New: {}",
            chain_name, collator_address, old, new
        );

        self.send(&message).await
    }

    /// Notify that an issue was resolved (detected by change in status)
    pub async fn notify_issue_resolved(
        &self,
        chain_name: &str,
        collator_address: &str,
        new_status: &str,
    ) -> Result<()> {
        // Only notify if there was an outstanding issue
        if !self.has_outstanding_issue(chain_name) && !self.was_manual_action_required(chain_name) {
            return Ok(());
        }

        self.remove_outstanding_issue(chain_name);
        self.clear_manual_action_required(chain_name);

        let message = format!(
            "✅ *Issue Resolved* on *{}*\n\n\
            Collator: `{}`\n\
            Current status: {}",
            chain_name, collator_address, new_status
        );

        self.send(&message).await
    }

    /// Send periodic status summary
    pub async fn send_status_summary(&self, slots: &[ChainSlotInfo]) -> Result<()> {
        let mut lines = vec!["📊 *Collator Slot Status Summary*\n".to_string()];

        for slot in slots {
            let status = if slot.is_invulnerable {
                "🛡️ Safe (Invulnerable)".to_string()
            } else if slot.is_candidate {
                if let (Some(pos), Some(max)) = (slot.position, slot.max_candidates) {
                    let distance_str = if let Some(dist) = slot.distance_from_last {
                        format!(
                            " | +{} {} from last",
                            format_balance(dist, slot.decimals, &slot.token_symbol),
                            slot.token_symbol
                        )
                    } else {
                        String::new()
                    };
                    
                    // Check if outside active set
                    let is_outside_active = max > 0 && pos > max as usize;
                    let position_str = format!("#{} of {} (max {})", pos, slot.total_candidates, max);
                    
                    if is_outside_active {
                        format!("⚠️ {}{} *OUTSIDE ACTIVE SET*", position_str, distance_str)
                    } else {
                        format!("✅ {}{}", position_str, distance_str)
                    }
                } else {
                    "✅ Candidate".to_string()
                }
            } else {
                "❌ Not a collator".to_string()
            };

            let block_time = if let Some(duration) = slot.last_block_time {
                format!(" | Last block: {} ago", Self::format_duration(duration))
            } else {
                String::new()
            };

            lines.push(format!("• *{}*: {}{}", slot.chain_name, status, block_time));
        }

        let message = lines.join("\n");
        self.send(&message).await
    }

    /// Notify about an error (for logging purposes)
    pub async fn notify_error(&self, chain_name: &str, error: &str) -> Result<()> {
        let message = format!(
            "❌ *Error* on *{}*\n\n`{}`",
            chain_name, error
        );
        self.send(&message).await
    }

    /// Alert that we cannot compete (bond too low)
    pub async fn alert_cannot_compete(
        &self,
        chain_name: &str,
        collator_address: &str,
        available_balance: u128,
        lowest_bond: u128,
        needed: u128,
        token_symbol: &str,
        decimals: u32,
    ) -> Result<()> {
        self.add_outstanding_issue(chain_name);

        let rate_key = format!("{}:cannot_compete", chain_name);
        if !self.should_notify(&rate_key) {
            return Ok(());
        }

        let available = format_balance(available_balance, decimals, token_symbol);
        let lowest = format_balance(lowest_bond, decimals, token_symbol);
        let need = format_balance(needed, decimals, token_symbol);
        let mentions = self.format_onchain_mentions();

        let message = format!(
            "⚠️ *Cannot Compete* on *{}*\n\n\
            Collator: `{}`\n\
            Available: {}\n\
            Lowest candidate bond: {}\n\
            Need at least: {} more\n\n\
            Please add funds to register as a candidate.{}\n\n\
            _This alert is rate-limited to once every 4 hours._",
            chain_name, collator_address, available, lowest, need, mentions
        );

        self.send(&message).await
    }

    /// Notify about successful registration (alias for notify_registration)
    pub async fn notify_registration_success(
        &self,
        chain_name: &str,
        collator_address: &str,
        bond_amount: u128,
        token_symbol: &str,
        decimals: u32,
    ) -> Result<()> {
        self.notify_registration(chain_name, collator_address, bond_amount, token_symbol, decimals).await
    }
}

/// Format a balance with proper decimals
fn format_balance(amount: u128, decimals: u32, symbol: &str) -> String {
    let divisor = 10u128.pow(decimals);
    let whole = amount / divisor;
    let frac = amount % divisor;
    let frac_str = format!("{:0>width$}", frac, width = decimals as usize);
    // Show 4 decimal places
    let display_frac = &frac_str[..4.min(frac_str.len())];
    format!("{}.{} {}", whole, display_frac, symbol)
}
