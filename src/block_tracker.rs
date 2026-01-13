//! Background block tracker for monitoring last authored blocks per chain.
//!
//! This module subscribes to new blocks on each chain and tracks when
//! the collator last authored a block.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use subxt::{OnlineClient, PolkadotConfig};
use subxt::utils::AccountId32;
use futures::StreamExt;

use crate::config::{default_rpc_url, AppConfig, Network, SystemChain};

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

/// Central tracker for all chain block authorship
pub struct BlockTracker {
    /// Map of chain name -> last block info
    data: Arc<RwLock<HashMap<String, LastBlockInfo>>>,
    /// Shutdown signal
    shutdown: Arc<RwLock<bool>>,
}

impl BlockTracker {
    /// Create a new block tracker
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
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
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_chain_tracker(network, chain, config).await;
        })
    }

    /// Run the tracker loop for a single chain
    async fn run_chain_tracker(
        self: Arc<Self>,
        network: Network,
        chain: SystemChain,
        config: AppConfig,
    ) {
        let chain_name = chain.display_name(network);
        let collator_address = config.collator_address(network);
        let rpc_url = config
            .chain_config(network, chain)
            .map(|c| c.rpc_url.clone())
            .unwrap_or_else(|| default_rpc_url(network, chain).to_string());

        info!("Starting block subscription for {}", chain_name);

        // Initialize tracking entry
        {
            let mut data = self.data.write().await;
            data.insert(chain_name.clone(), LastBlockInfo::new());
        }

        // Reconnection loop
        loop {
            if self.is_shutdown().await {
                info!("Block tracker for {} shutting down", chain_name);
                break;
            }

            match self.subscribe_to_blocks(&chain_name, &rpc_url, collator_address).await {
                Ok(()) => {
                    // Subscription ended normally (shutdown)
                    break;
                }
                Err(e) => {
                    warn!("Block subscription for {} failed: {}. Reconnecting in 30s...", chain_name, e);
                    self.mark_disconnected(&chain_name, e.to_string()).await;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        }
    }

    /// Subscribe to new blocks and track authorship
    async fn subscribe_to_blocks(
        &self,
        chain_name: &str,
        rpc_url: &str,
        collator_address: &str,
    ) -> anyhow::Result<()> {
        // Connect to the chain
        let api = OnlineClient::<PolkadotConfig>::from_url(rpc_url).await?;
        info!("Connected to {} for block tracking", chain_name);
        
        // Parse collator address
        let collator_account: AccountId32 = collator_address.parse()
            .map_err(|e| anyhow::anyhow!("Invalid collator address: {}", e))?;

        // Subscribe to finalized blocks
        let mut block_sub = api.blocks().subscribe_finalized().await?;
        
        self.mark_connected(chain_name).await;

        while let Some(block_result) = block_sub.next().await {
            if self.is_shutdown().await {
                return Ok(());
            }

            match block_result {
                Ok(block) => {
                    // Refresh authorities for each block (they can change)
                    let authorities = match self.get_aura_authorities(&api, &block).await {
                        Ok(a) => a,
                        Err(e) => {
                            debug!("{}: Failed to get authorities: {}", chain_name, e);
                            continue;
                        }
                    };

                    // Check if our collator authored this block
                    if let Some(author) = self.get_block_author_from_digest(&block, &authorities) {
                        if author == collator_account {
                            info!("{}: Authored block #{}", chain_name, block.number());
                            self.record_authored_block(chain_name).await;
                        }
                    }
                }
                Err(e) => {
                    warn!("{}: Block subscription error: {}", chain_name, e);
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    /// Get Aura authorities from chain storage at a specific block
    async fn get_aura_authorities(
        &self, 
        api: &OnlineClient<PolkadotConfig>,
        block: &subxt::blocks::Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    ) -> anyhow::Result<Vec<AccountId32>> {
        // Try Aura.Authorities first
        let storage_query = subxt::dynamic::storage("Aura", "Authorities", ());
        let result = api.storage().at(block.reference()).fetch(&storage_query).await?;
        
        if let Some(value) = result {
            let decoded = value.to_value()?;
            let authorities = parse_authorities_from_value(&decoded)?;
            if !authorities.is_empty() {
                return Ok(authorities);
            }
        }

        // Fallback: Try Session.Validators
        let storage_query = subxt::dynamic::storage("Session", "Validators", ());
        let result = api.storage().at(block.reference()).fetch(&storage_query).await?;
        
        if let Some(value) = result {
            let decoded = value.to_value()?;
            let authorities = parse_authorities_from_value(&decoded)?;
            if !authorities.is_empty() {
                return Ok(authorities);
            }
        }

        Ok(Vec::new())
    }

    /// Get block author from the Aura pre-runtime digest
    fn get_block_author_from_digest(
        &self,
        block: &subxt::blocks::Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
        authorities: &[AccountId32],
    ) -> Option<AccountId32> {
        if authorities.is_empty() {
            return None;
        }

        let header = block.header();
        for log in header.digest.logs.iter() {
            if let subxt::config::substrate::DigestItem::PreRuntime(engine_id, data) = log {
                // Aura engine ID is *b"aura"
                if engine_id == b"aura" && data.len() >= 8 {
                    // Slot is encoded as u64 LE
                    let slot = u64::from_le_bytes(data[0..8].try_into().ok()?);
                    let author_index = (slot as usize) % authorities.len();
                    return Some(authorities[author_index].clone());
                }
            }
        }

        None
    }
}

impl Default for BlockTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse authorities from a dynamic scale value
fn parse_authorities_from_value<T: std::fmt::Debug>(
    value: &subxt::ext::scale_value::Value<T>
) -> anyhow::Result<Vec<AccountId32>> {
    use subxt::ext::scale_value::{ValueDef, Composite, Primitive};
    
    let mut authorities = Vec::new();
    
    fn try_extract_account<T: std::fmt::Debug>(
        value: &subxt::ext::scale_value::Value<T>
    ) -> Option<AccountId32> {
        match &value.value {
            // Direct 32-byte array
            ValueDef::Composite(Composite::Unnamed(bytes)) if bytes.len() == 32 => {
                let mut account_bytes = [0u8; 32];
                for (i, b) in bytes.iter().enumerate() {
                    if let ValueDef::Primitive(Primitive::U128(n)) = &b.value {
                        account_bytes[i] = *n as u8;
                    } else {
                        return None;
                    }
                }
                Some(AccountId32(account_bytes))
            }
            // Newtype wrapper with single element
            ValueDef::Composite(Composite::Unnamed(items)) if items.len() == 1 => {
                try_extract_account(&items[0])
            }
            // Named struct with "0" or similar field
            ValueDef::Composite(Composite::Named(fields)) => {
                for (name, val) in fields {
                    if name == "0" || name.to_lowercase().contains("inner") {
                        return try_extract_account(val);
                    }
                }
                None
            }
            _ => None,
        }
    }
    
    fn extract_all_accounts<T: std::fmt::Debug>(
        value: &subxt::ext::scale_value::Value<T>,
        accounts: &mut Vec<AccountId32>,
    ) {
        match &value.value {
            ValueDef::Composite(Composite::Unnamed(items)) => {
                // Check if this is a single newtype wrapper
                if items.len() == 1 {
                    // Try to extract as account first
                    if let Some(account) = try_extract_account(value) {
                        accounts.push(account);
                    } else {
                        // Recurse into the wrapper
                        extract_all_accounts(&items[0], accounts);
                    }
                } else {
                    // This is likely the actual list of accounts
                    for item in items {
                        if let Some(account) = try_extract_account(item) {
                            accounts.push(account);
                        }
                    }
                }
            }
            ValueDef::Composite(Composite::Named(fields)) => {
                // Look for "0" field which indicates newtype wrapper
                for (name, val) in fields {
                    if name == "0" {
                        extract_all_accounts(val, accounts);
                        return;
                    }
                }
            }
            _ => {}
        }
    }
    
    extract_all_accounts(value, &mut authorities);
    Ok(authorities)
}
