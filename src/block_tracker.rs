//! Background block tracker for monitoring last authored blocks per chain.
//!
//! This module subscribes to new blocks on each chain and tracks when
//! the collator last authored a block using typed metadata for proper
//! Aura slot and Session key owner lookups.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use subxt::{OnlineClient, PolkadotConfig};
use subxt::utils::AccountId32;
use futures::StreamExt;
use parity_scale_codec::Encode;

use crate::config::{default_rpc_url, AppConfig, Network, SystemChain};

// ====== Subxt metadata modules (one per chain) ======
#[subxt::subxt(runtime_metadata_path = "metadata/asset-hub-polkadot.scale")]
pub mod asset_hub_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/asset-hub-kusama.scale")]
pub mod asset_hub_kusama {}

#[subxt::subxt(runtime_metadata_path = "metadata/bridge-hub-polkadot.scale")]
pub mod bridge_hub_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/bridge-hub-kusama.scale")]
pub mod bridge_hub_kusama {}

#[subxt::subxt(runtime_metadata_path = "metadata/collectives-polkadot.scale")]
pub mod collectives_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/coretime-polkadot.scale")]
pub mod coretime_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/coretime-kusama.scale")]
pub mod coretime_kusama {}

#[subxt::subxt(runtime_metadata_path = "metadata/people-polkadot.scale")]
pub mod people_polkadot {}

#[subxt::subxt(runtime_metadata_path = "metadata/people-kusama.scale")]
pub mod people_kusama {}

#[subxt::subxt(runtime_metadata_path = "metadata/encointer-kusama.scale")]
pub mod encointer_kusama {}

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

        // Parse collator address once
        let collator_account: AccountId32 = match collator_address.parse() {
            Ok(acc) => acc,
            Err(e) => {
                warn!("Invalid collator address for {}: {}", chain_name, e);
                return;
            }
        };

        // Reconnection loop
        loop {
            if self.is_shutdown().await {
                info!("Block tracker for {} shutting down", chain_name);
                break;
            }

            match self.subscribe_to_blocks_typed(
                &chain_name, 
                &rpc_url, 
                network, 
                chain, 
                &collator_account
            ).await {
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

    /// Subscribe to new blocks using typed metadata
    async fn subscribe_to_blocks_typed(
        &self,
        chain_name: &str,
        rpc_url: &str,
        network: Network,
        chain: SystemChain,
        collator_account: &AccountId32,
    ) -> anyhow::Result<()> {
        // Connect to the chain
        let api = OnlineClient::<PolkadotConfig>::from_url(rpc_url).await?;
        info!("Connected to {} for block tracking", chain_name);
        
        // Subscribe to finalized blocks
        let mut block_sub = api.blocks().subscribe_finalized().await?;
        
        self.mark_connected(chain_name).await;

        while let Some(block_result) = block_sub.next().await {
            if self.is_shutdown().await {
                return Ok(());
            }

            match block_result {
                Ok(block) => {
                    let block_hash = block.hash();
                    
                    // Get the block author using typed storage queries
                    let author = get_block_author_typed(&api, block_hash, network, chain).await;
                    
                    if let Some(author_account) = author {
                        if author_account == *collator_account {
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
}

impl Default for BlockTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a runtime AccountId32 (opaque newtype) into [u8;32] by SCALE-encoding then truncating.
fn account_to_raw32<T: Encode>(acc: T) -> [u8; 32] {
    let bytes = acc.encode();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[..32]);
    out
}

/// Get block author using typed storage queries for each chain
async fn get_block_author_typed(
    api: &OnlineClient<PolkadotConfig>,
    block_hash: subxt::utils::H256,
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
