//! Background block tracker for monitoring last authored blocks per chain.
//!
//! Uses typed metadata to correctly derive block authors from Aura.CurrentSlot,
//! Aura.Authorities, and Session.KeyOwner.
//!
//! Also monitors collator status changes and alerts when our collator is removed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn, error};
use subxt::{OnlineClient, PolkadotConfig};
use subxt::config::substrate::H256;
use subxt::utils::AccountId32;
use futures::StreamExt;
use parity_scale_codec::Encode;

use crate::config::{AppConfig, Network, SystemChain};
use crate::slack::SlackNotifier;
use crate::metadata::*;

/// Tracks last authored block times for all chains
#[derive(Debug, Clone)]
pub struct LastBlockInfo {
    /// When the collator last authored a block (None if never seen)
    pub last_authored: Option<Instant>,
    /// When this tracker started (to know if "never seen" is meaningful)
    pub tracking_since: Instant,
    /// Whether the tracker is currently connected
    pub is_connected: bool,
    /// Last error message if any
    pub last_error: Option<String>,
}

impl LastBlockInfo {
    fn new() -> Self {
        Self {
            last_authored: None,
            tracking_since: Instant::now(),
            is_connected: false,
            last_error: None,
        }
    }
    
    /// Get time since last authored block
    pub fn time_since_last_block(&self) -> Option<Duration> {
        self.last_authored.map(|t| t.elapsed())
    }
}

/// Collator status for monitoring
#[derive(Debug, Clone, PartialEq)]
pub enum TrackedCollatorStatus {
    Invulnerable,
    Candidate { deposit: u128 },
    NotCollator,
    Unknown,
}

/// Central tracker for all chain block authorship
pub struct BlockTracker {
    /// Map of chain name -> last block info
    data: Arc<RwLock<HashMap<String, LastBlockInfo>>>,
    /// Map of chain name -> last known collator status
    collator_status: Arc<RwLock<HashMap<String, TrackedCollatorStatus>>>,
    /// Shutdown signal
    shutdown: Arc<RwLock<bool>>,
}

impl BlockTracker {
    /// Create a new block tracker
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
            collator_status: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(RwLock::new(false)),
        }
    }

    /// Get the last block info for a chain
    pub async fn get_last_block(&self, chain_name: &str) -> Option<LastBlockInfo> {
        let data = self.data.read().await;
        data.get(chain_name).cloned()
    }

    /// Record that the collator authored a block
    async fn record_authored_block(&self, chain_name: &str) {
        let mut data = self.data.write().await;
        if let Some(info) = data.get_mut(chain_name) {
            info.last_authored = Some(Instant::now());
            info.is_connected = true;
            info.last_error = None;
        }
    }

    /// Mark chain as connected (receiving blocks but not authoring)
    async fn mark_connected(&self, chain_name: &str) {
        let mut data = self.data.write().await;
        if let Some(info) = data.get_mut(chain_name) {
            info.is_connected = true;
            info.last_error = None;
        }
    }

    /// Mark chain as disconnected with error
    async fn mark_disconnected(&self, chain_name: &str, error: String) {
        let mut data = self.data.write().await;
        if let Some(info) = data.get_mut(chain_name) {
            info.is_connected = false;
            info.last_error = Some(error);
        }
    }

    /// Update tracked collator status
    async fn update_collator_status(&self, chain_name: &str, status: TrackedCollatorStatus) -> Option<TrackedCollatorStatus> {
        let mut statuses = self.collator_status.write().await;
        let old = statuses.get(chain_name).cloned();
        statuses.insert(chain_name.to_string(), status);
        old
    }

    /// Signal shutdown
    pub async fn shutdown(&self) {
        let mut shutdown = self.shutdown.write().await;
        *shutdown = true;
    }

    /// Check if shutdown was requested
    async fn is_shutdown(&self) -> bool {
        let shutdown = self.shutdown.read().await;
        *shutdown
    }

    /// Start background tracking for all chains
    pub fn start_tracking(
        self: Arc<Self>,
        config: AppConfig,
        slack: Arc<SlackNotifier>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let mut handles = Vec::new();

        let polkadot_chains = [
            SystemChain::AssetHub,
            SystemChain::BridgeHub,
            SystemChain::Collectives,
            SystemChain::Coretime,
            SystemChain::People,
        ];

        let kusama_chains = [
            SystemChain::AssetHub,
            SystemChain::BridgeHub,
            SystemChain::Coretime,
            SystemChain::People,
            SystemChain::Encointer,
        ];

        // Start Polkadot chain trackers
        for chain in polkadot_chains {
            if chain.valid_networks().contains(&Network::Polkadot) {
                let handle = self.clone().spawn_chain_tracker(
                    Network::Polkadot,
                    chain,
                    config.clone(),
                    slack.clone(),
                );
                handles.push(handle);
            }
        }

        // Start Kusama chain trackers
        for chain in kusama_chains {
            if chain.valid_networks().contains(&Network::Kusama) {
                let handle = self.clone().spawn_chain_tracker(
                    Network::Kusama,
                    chain,
                    config.clone(),
                    slack.clone(),
                );
                handles.push(handle);
            }
        }

        info!("Started {} background block trackers", handles.len());
        handles
    }

    /// Spawn a tracker for a single chain
    fn spawn_chain_tracker(
        self: Arc<Self>,
        network: Network,
        chain: SystemChain,
        config: AppConfig,
        slack: Arc<SlackNotifier>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_chain_tracker(network, chain, config, slack).await;
        })
    }

    /// Try to connect to any of the provided RPC URLs
    /// Returns (api, connected_url_index) on success, or None if all fail
    async fn try_connect_to_any(
        chain_name: &str,
        rpc_urls: &[String],
    ) -> Option<(OnlineClient<PolkadotConfig>, usize)> {
        for (idx, url) in rpc_urls.iter().enumerate() {
            match OnlineClient::<PolkadotConfig>::from_url(url).await {
                Ok(api) => {
                    if idx > 0 {
                        // Connected to a fallback - log this
                        info!("{}: Connected to fallback RPC #{} ({})", chain_name, idx + 1, url);
                    } else {
                        info!("{}: Connected to primary RPC ({})", chain_name, url);
                    }
                    return Some((api, idx));
                }
                Err(e) => {
                    // Log to console but don't alert Slack yet
                    warn!("{}: Failed to connect to RPC #{} ({}): {}", chain_name, idx + 1, url, e);
                }
            }
        }
        None
    }

    /// Run the tracker loop for a single chain with reconnection handling
    async fn run_chain_tracker(
        self: Arc<Self>,
        network: Network,
        chain: SystemChain,
        config: AppConfig,
        slack: Arc<SlackNotifier>,
    ) {
        let chain_name = chain.display_name(network);
        let collator_address = config.collator_address(network);
        let rpc_urls = config.get_rpc_urls(network, chain);

        info!("Starting block subscription for {} with {} RPC endpoints", chain_name, rpc_urls.len());
        for (i, url) in rpc_urls.iter().enumerate() {
            debug!("  RPC #{}: {}", i + 1, url);
        }

        // Initialize tracking entry
        {
            let mut data = self.data.write().await;
            data.insert(chain_name.clone(), LastBlockInfo::new());
        }

        // Parse collator address once
        let collator_account: AccountId32 = match collator_address.parse() {
            Ok(acc) => acc,
            Err(e) => {
                error!("Invalid collator address for {}: {}", chain_name, e);
                return;
            }
        };

        // Track which RPC we're currently using (for logging)
        let mut current_rpc_idx: usize;

        // Reconnection loop
        loop {
            if self.is_shutdown().await {
                info!("Block tracker for {} shutting down", chain_name);
                break;
            }

            // Try to connect to any available RPC
            let api = match Self::try_connect_to_any(&chain_name, &rpc_urls).await {
                Some((api, idx)) => {
                    // Successfully connected - clear any Slack alert
                    slack.report_reconnect(&chain_name).await;
                    current_rpc_idx = idx;
                    api
                }
                None => {
                    // ALL RPCs failed - now alert Slack
                    let error_msg = format!("All {} RPC endpoints failed", rpc_urls.len());
                    error!("{}: {}", chain_name, error_msg);
                    slack.report_disconnect(&chain_name, &error_msg).await;
                    self.mark_disconnected(&chain_name, error_msg).await;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
            };

            // Try to subscribe to finalized blocks
            let mut block_sub = match api.blocks().subscribe_finalized().await {
                Ok(sub) => {
                    self.mark_connected(&chain_name).await;
                    sub
                }
                Err(e) => {
                    warn!("{}: Subscription failed on RPC #{}: {}", chain_name, current_rpc_idx + 1, e);
                    // Don't alert Slack yet - try other RPCs first
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            // Track last block check time for block production alerts
            let mut last_block_alert_check = Instant::now();
            const BLOCK_ALERT_THRESHOLD: Duration = Duration::from_secs(30 * 60); // 30 minutes
            const BLOCK_ALERT_CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60); // Check every 30 min

            // Process blocks
            while let Some(block_result) = block_sub.next().await {
                if self.is_shutdown().await {
                    return;
                }

                match block_result {
                    Ok(block) => {
                        let block_number = block.number();
                        let block_hash = block.hash();
                        
                        // Check block author
                        match get_block_author_typed(&api, block_hash, network, chain).await {
                            Some(author) if author == collator_account => {
                                info!("{}: Authored block #{}", chain_name, block_number);
                                self.record_authored_block(&chain_name).await;
                                
                                // Clear any block production alert
                                slack.report_block_authored(&chain_name).await;
                            }
                            Some(_) => {
                                // Not our block, but connection is good
                            }
                            None => {
                                debug!("{}: Could not determine block author for #{}", chain_name, block_number);
                            }
                        }

                        // Check for block production alerts (every 30 min)
                        if last_block_alert_check.elapsed() >= BLOCK_ALERT_CHECK_INTERVAL {
                            last_block_alert_check = Instant::now();
                            
                            // Check if we haven't authored a block in 30 minutes
                            if let Some(info) = self.get_last_block(&chain_name).await {
                                if let Some(duration) = info.time_since_last_block() {
                                    if duration >= BLOCK_ALERT_THRESHOLD {
                                        slack.report_no_blocks(&chain_name, duration).await;
                                    }
                                } else if info.tracking_since.elapsed() >= BLOCK_ALERT_THRESHOLD {
                                    // Never authored since we started tracking
                                    slack.report_no_blocks(&chain_name, info.tracking_since.elapsed()).await;
                                }
                            }
                        }

                        // Check collator status changes
                        if let Err(e) = self.check_collator_status(
                            &api, 
                            &chain_name, 
                            network, 
                            chain, 
                            &collator_account,
                            block_number,
                            block_hash,
                            &slack,
                        ).await {
                            debug!("{}: Error checking collator status: {}", chain_name, e);
                        }
                    }
                    Err(e) => {
                        // Log to console but don't alert Slack - will try fallback RPCs
                        warn!("{}: Block stream error on RPC #{}: {}. Will try reconnecting...", 
                            chain_name, current_rpc_idx + 1, e);
                        self.mark_disconnected(&chain_name, e.to_string()).await;
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        break; // Break to reconnect (will try all RPCs)
                    }
                }
            }

            // Stream ended without error - log but don't alert Slack yet
            warn!("{}: Block stream ended on RPC #{}. Will try reconnecting...", 
                chain_name, current_rpc_idx + 1);
            
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Check if our collator status changed and alert if removed
    async fn check_collator_status(
        &self,
        api: &OnlineClient<PolkadotConfig>,
        chain_name: &str,
        network: Network,
        chain: SystemChain,
        collator_account: &AccountId32,
        block_number: u32,
        block_hash: H256,
        slack: &SlackNotifier,
    ) -> anyhow::Result<()> {
        // Get current status from chain
        let current_status = get_collator_status_typed(api, block_hash, collator_account).await?;
        
        // Get previous status
        let old_status = self.update_collator_status(chain_name, current_status.clone()).await;
        
        // Check for status change
        if let Some(old) = old_status {
            if old != TrackedCollatorStatus::Unknown && old != current_status {
                // Status changed!
                let subscan_base = subscan_base_for_chain(network, chain);
                let block_url = format!("{}/block/{}", subscan_base, block_number);
                
                match (&old, &current_status) {
                    (TrackedCollatorStatus::Invulnerable, TrackedCollatorStatus::NotCollator) |
                    (TrackedCollatorStatus::Candidate { .. }, TrackedCollatorStatus::NotCollator) => {
                        // We were removed!
                        let msg = format!(
                            "🚨 *COLLATOR REMOVED* on *{}*\n\n\
                            Our collator was removed at block #{}\n\
                            Previous status: {:?}\n\
                            Current status: Not a collator\n\n\
                            Check the block for transactions that affected us:\n\
                            {}",
                            chain_name, block_number, old, block_url
                        );
                        error!("{}", msg);
                        let _ = slack.send_alert(&msg).await;
                    }
                    (TrackedCollatorStatus::Invulnerable, TrackedCollatorStatus::Candidate { deposit }) => {
                        // Moved from invulnerable to candidate
                        let msg = format!(
                            "⚠️ *Status Change* on *{}*\n\n\
                            Moved from Invulnerable to Candidate at block #{}\n\
                            Bond: {} {}\n\n\
                            Block: {}",
                            chain_name, block_number, 
                            format_balance(*deposit, network),
                            network.symbol(),
                            block_url
                        );
                        warn!("{}", msg);
                        let _ = slack.send_alert(&msg).await;
                    }
                    (TrackedCollatorStatus::NotCollator, TrackedCollatorStatus::Candidate { .. }) |
                    (TrackedCollatorStatus::NotCollator, TrackedCollatorStatus::Invulnerable) => {
                        // We were added (good news)
                        info!("{}: Collator status changed from {:?} to {:?} at block #{}", 
                            chain_name, old, current_status, block_number);
                    }
                    _ => {
                        // Other changes (bond updates, etc.)
                        debug!("{}: Collator status changed from {:?} to {:?}", 
                            chain_name, old, current_status);
                    }
                }
            }
        }
        
        Ok(())
    }
}

impl Default for BlockTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Format balance for display
fn format_balance(amount: u128, network: Network) -> String {
    let decimals = network.decimals();
    let divisor = 10u128.pow(decimals);
    let whole = amount / divisor;
    let frac = amount % divisor;
    format!("{}.{:04}", whole, frac / 10u128.pow(decimals - 4))
}

/// Get Subscan base URL for a chain
fn subscan_base_for_chain(network: Network, chain: SystemChain) -> &'static str {
    match (network, chain) {
        (Network::Polkadot, SystemChain::AssetHub) => "https://assethub-polkadot.subscan.io",
        (Network::Polkadot, SystemChain::BridgeHub) => "https://bridgehub-polkadot.subscan.io",
        (Network::Polkadot, SystemChain::Collectives) => "https://collectives-polkadot.subscan.io",
        (Network::Polkadot, SystemChain::Coretime) => "https://coretime-polkadot.subscan.io",
        (Network::Polkadot, SystemChain::People) => "https://people-polkadot.subscan.io",
        (Network::Kusama, SystemChain::AssetHub) => "https://assethub-kusama.subscan.io",
        (Network::Kusama, SystemChain::BridgeHub) => "https://bridgehub-kusama.subscan.io",
        (Network::Kusama, SystemChain::Coretime) => "https://coretime-kusama.subscan.io",
        (Network::Kusama, SystemChain::People) => "https://people-kusama.subscan.io",
        (Network::Kusama, SystemChain::Encointer) => "https://encointer-kusama.subscan.io",
        _ => "https://polkadot.subscan.io",
    }
}

/// Get collator status using dynamic storage queries
async fn get_collator_status_typed(
    api: &OnlineClient<PolkadotConfig>,
    block_hash: H256,
    collator_account: &AccountId32,
) -> anyhow::Result<TrackedCollatorStatus> {
    let collator_bytes = collator_account.0;
    
    // Check invulnerables first
    let invuln_query = subxt::dynamic::storage("CollatorSelection", "Invulnerables", ());
    if let Ok(Some(value)) = api.storage().at(block_hash).fetch(&invuln_query).await {
        if let Ok(decoded) = value.to_value() {
            if contains_account(&decoded, &collator_bytes) {
                return Ok(TrackedCollatorStatus::Invulnerable);
            }
        }
    }
    
    // Check candidates
    let cand_query = subxt::dynamic::storage("CollatorSelection", "CandidateList", ());
    if let Ok(Some(value)) = api.storage().at(block_hash).fetch(&cand_query).await {
        if let Ok(decoded) = value.to_value() {
            if let Some(deposit) = find_candidate_deposit(&decoded, &collator_bytes) {
                return Ok(TrackedCollatorStatus::Candidate { deposit });
            }
        }
    }
    
    Ok(TrackedCollatorStatus::NotCollator)
}

/// Check if a decoded value contains an account
fn contains_account<T: std::fmt::Debug>(value: &subxt::ext::scale_value::Value<T>, account: &[u8; 32]) -> bool {
    use subxt::ext::scale_value::{ValueDef, Composite, Primitive};
    
    fn check<T: std::fmt::Debug>(value: &subxt::ext::scale_value::Value<T>, account: &[u8; 32]) -> bool {
        match &value.value {
            ValueDef::Composite(Composite::Unnamed(items)) => {
                // Could be an account or a list
                if items.len() == 32 {
                    // Might be account bytes
                    let mut bytes = [0u8; 32];
                    for (i, item) in items.iter().enumerate() {
                        if let ValueDef::Primitive(Primitive::U128(n)) = &item.value {
                            bytes[i] = *n as u8;
                        } else {
                            return items.iter().any(|item| check(item, account));
                        }
                    }
                    return &bytes == account;
                }
                items.iter().any(|item| check(item, account))
            }
            ValueDef::Composite(Composite::Named(fields)) => {
                fields.iter().any(|(_, val)| check(val, account))
            }
            _ => false,
        }
    }
    
    check(value, account)
}

/// Find candidate deposit for an account
fn find_candidate_deposit<T: std::fmt::Debug>(value: &subxt::ext::scale_value::Value<T>, account: &[u8; 32]) -> Option<u128> {
    use subxt::ext::scale_value::{ValueDef, Composite, Primitive};
    
    fn find<T: std::fmt::Debug>(value: &subxt::ext::scale_value::Value<T>, account: &[u8; 32]) -> Option<u128> {
        match &value.value {
            ValueDef::Composite(Composite::Unnamed(items)) => {
                // This could be the candidates list
                for item in items {
                    if let Some(deposit) = find(item, account) {
                        return Some(deposit);
                    }
                }
                None
            }
            ValueDef::Composite(Composite::Named(fields)) => {
                // This could be a CandidateInfo struct
                let mut found_account = false;
                let mut deposit = None;
                
                for (name, val) in fields {
                    if name == "who" || name == "0" {
                        if contains_account(val, account) {
                            found_account = true;
                        }
                    }
                    if name == "deposit" || name == "1" {
                        if let ValueDef::Primitive(Primitive::U128(d)) = &val.value {
                            deposit = Some(*d);
                        }
                    }
                }
                
                if found_account {
                    return deposit;
                }
                
                // Recurse into fields
                for (_, val) in fields {
                    if let Some(d) = find(val, account) {
                        return Some(d);
                    }
                }
                None
            }
            _ => None,
        }
    }
    
    find(value, account)
}

/// Convert our RawAccountId ([u8;32]) to [u8;32].
fn account_to_raw32<T: Encode>(acc: T) -> [u8; 32] {
    let bytes = acc.encode();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[..32]);
    out
}

/// Get block author using typed storage queries for each chain
async fn get_block_author_typed(
    api: &OnlineClient<PolkadotConfig>,
    block_hash: H256,
    network: Network,
    chain: SystemChain,
) -> Option<AccountId32> {
    let aura_key_type = *b"aura";
    
    // Macro to reduce boilerplate for each chain
    macro_rules! get_author {
        ($mod:ident, $key_type:ty) => {{
            // Get current slot
            let slot_query = $mod::storage().aura().current_slot();
            let slot: Option<$mod::runtime_types::sp_consensus_slots::Slot> = 
                api.storage().at(block_hash).fetch(&slot_query).await.ok()?;
            
            // Get authorities
            let auths_query = $mod::storage().aura().authorities();
            let auths: Option<$mod::runtime_types::bounded_collections::bounded_vec::BoundedVec<$key_type>> =
                api.storage().at(block_hash).fetch(&auths_query).await.ok()?;
            
            if let (Some(slot), Some(auths)) = (slot, auths) {
                let authorities = auths.0;
                if authorities.is_empty() {
                    return None;
                }
                
                let idx = (slot.0 as usize) % authorities.len();
                let aura_key = authorities[idx].0;
                
                // Look up the owner via Session.KeyOwner
                let key_type = $mod::runtime_types::sp_core::crypto::KeyTypeId(aura_key_type);
                let owner_query = $mod::storage().session().key_owner((key_type, aura_key.to_vec()));
                let owner: Option<_> = api.storage().at(block_hash).fetch(&owner_query).await.ok()?;
                
                owner.map(|o| AccountId32(account_to_raw32(o)))
            } else {
                None
            }
        }};
    }

    match (network, chain) {
        // Polkadot chains
        (Network::Polkadot, SystemChain::AssetHub) => {
            // Asset Hub Polkadot uses ed25519 for Aura
            get_author!(asset_hub_polkadot, asset_hub_polkadot::runtime_types::sp_consensus_aura::ed25519::app_ed25519::Public)
        }
        (Network::Polkadot, SystemChain::BridgeHub) => {
            get_author!(bridge_hub_polkadot, bridge_hub_polkadot::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public)
        }
        (Network::Polkadot, SystemChain::Collectives) => {
            get_author!(collectives_polkadot, collectives_polkadot::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public)
        }
        (Network::Polkadot, SystemChain::Coretime) => {
            get_author!(coretime_polkadot, coretime_polkadot::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public)
        }
        (Network::Polkadot, SystemChain::People) => {
            get_author!(people_polkadot, people_polkadot::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public)
        }
        // Kusama chains
        (Network::Kusama, SystemChain::AssetHub) => {
            get_author!(asset_hub_kusama, asset_hub_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public)
        }
        (Network::Kusama, SystemChain::BridgeHub) => {
            get_author!(bridge_hub_kusama, bridge_hub_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public)
        }
        (Network::Kusama, SystemChain::Coretime) => {
            get_author!(coretime_kusama, coretime_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public)
        }
        (Network::Kusama, SystemChain::People) => {
            get_author!(people_kusama, people_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public)
        }
        (Network::Kusama, SystemChain::Encointer) => {
            get_author!(encointer_kusama, encointer_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public)
        }
        _ => None,
    }
}
