//! Background block tracker for monitoring last authored blocks per chain.
//!
//! Uses typed metadata to correctly derive block authors from Aura.CurrentSlot,
//! Aura.Authorities, and Session.KeyOwner.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use subxt::{OnlineClient, PolkadotConfig};
use subxt::config::substrate::H256;
use futures::StreamExt;
use parity_scale_codec::Encode;

use crate::config::{default_rpc_url, AppConfig, Network, SystemChain};
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

            match self.subscribe_to_blocks(&chain_name, &rpc_url, collator_address, network, chain).await {
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
        network: Network,
        chain: SystemChain,
    ) -> anyhow::Result<()> {
        // Connect to the chain
        let api = OnlineClient::<PolkadotConfig>::from_url(rpc_url).await?;
        info!("Connected to {} for block tracking", chain_name);
        
        // Parse collator address to raw bytes
        let collator_raw = parse_ss58_to_raw32(collator_address)?;

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
                    
                    // Get block author using typed queries
                    match self.get_block_author_typed(&api, block_hash, network, chain).await {
                        Ok(Some(author_raw)) => {
                            if author_raw == collator_raw {
                                info!("{}: Authored block #{}", chain_name, block.number());
                                self.record_authored_block(chain_name).await;
                            }
                        }
                        Ok(None) => {
                            debug!("{}: Could not determine block author for #{}", chain_name, block.number());
                        }
                        Err(e) => {
                            debug!("{}: Error getting block author: {}", chain_name, e);
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

    /// Get block author using typed metadata queries
    /// Returns the raw 32-byte account ID of the block author
    async fn get_block_author_typed(
        &self,
        api: &OnlineClient<PolkadotConfig>,
        block_hash: H256,
        network: Network,
        chain: SystemChain,
    ) -> anyhow::Result<Option<[u8; 32]>> {
        // First get the session key that authored this block
        let session_key = match self.derive_session_key_typed(api, block_hash, network, chain).await? {
            Some(k) => k,
            None => return Ok(None),
        };
        
        // Then look up the owner of that session key
        self.session_key_owner_typed(api, block_hash, network, chain, session_key).await
    }

    /// Derive the session key (Aura public key) for the block author
    async fn derive_session_key_typed(
        &self,
        api: &OnlineClient<PolkadotConfig>,
        at: H256,
        network: Network,
        chain: SystemChain,
    ) -> anyhow::Result<Option<[u8; 32]>> {
        // Macro to extract session key from slot and authorities
        macro_rules! pick_key {
            ($slot_opt:expr, $auths_opt:expr) => {{
                if let (Some(slot), Some(bv)) = ($slot_opt, $auths_opt) {
                    let v = bv.0;
                    if v.is_empty() { 
                        None 
                    } else {
                        let idx = (slot.0 as usize) % v.len();
                        Some(v[idx].0)
                    }
                } else { 
                    None 
                }
            }};
        }

        let key_opt = match (network, chain) {
            // Polkadot chains
            (Network::Polkadot, SystemChain::AssetHub) => {
                let slot: Option<asset_hub_polkadot::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&asset_hub_polkadot::storage().aura().current_slot()).await?;
                let auths: Option<
                    asset_hub_polkadot::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        asset_hub_polkadot::runtime_types::sp_consensus_aura::ed25519::app_ed25519::Public
                    >
                > = api.storage().at(at).fetch(&asset_hub_polkadot::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            (Network::Polkadot, SystemChain::BridgeHub) => {
                let slot: Option<bridge_hub_polkadot::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&bridge_hub_polkadot::storage().aura().current_slot()).await?;
                let auths: Option<
                    bridge_hub_polkadot::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        bridge_hub_polkadot::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public
                    >
                > = api.storage().at(at).fetch(&bridge_hub_polkadot::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            (Network::Polkadot, SystemChain::Collectives) => {
                let slot: Option<collectives_polkadot::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&collectives_polkadot::storage().aura().current_slot()).await?;
                let auths: Option<
                    collectives_polkadot::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        collectives_polkadot::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public
                    >
                > = api.storage().at(at).fetch(&collectives_polkadot::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            (Network::Polkadot, SystemChain::Coretime) => {
                let slot: Option<coretime_polkadot::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&coretime_polkadot::storage().aura().current_slot()).await?;
                let auths: Option<
                    coretime_polkadot::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        coretime_polkadot::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public
                    >
                > = api.storage().at(at).fetch(&coretime_polkadot::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            (Network::Polkadot, SystemChain::People) => {
                let slot: Option<people_polkadot::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&people_polkadot::storage().aura().current_slot()).await?;
                let auths: Option<
                    people_polkadot::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        people_polkadot::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public
                    >
                > = api.storage().at(at).fetch(&people_polkadot::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            // Kusama chains
            (Network::Kusama, SystemChain::AssetHub) => {
                let slot: Option<asset_hub_kusama::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&asset_hub_kusama::storage().aura().current_slot()).await?;
                let auths: Option<
                    asset_hub_kusama::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        asset_hub_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public
                    >
                > = api.storage().at(at).fetch(&asset_hub_kusama::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            (Network::Kusama, SystemChain::BridgeHub) => {
                let slot: Option<bridge_hub_kusama::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&bridge_hub_kusama::storage().aura().current_slot()).await?;
                let auths: Option<
                    bridge_hub_kusama::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        bridge_hub_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public
                    >
                > = api.storage().at(at).fetch(&bridge_hub_kusama::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            (Network::Kusama, SystemChain::Coretime) => {
                let slot: Option<coretime_kusama::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&coretime_kusama::storage().aura().current_slot()).await?;
                let auths: Option<
                    coretime_kusama::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        coretime_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public
                    >
                > = api.storage().at(at).fetch(&coretime_kusama::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            (Network::Kusama, SystemChain::People) => {
                let slot: Option<people_kusama::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&people_kusama::storage().aura().current_slot()).await?;
                let auths: Option<
                    people_kusama::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        people_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public
                    >
                > = api.storage().at(at).fetch(&people_kusama::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            (Network::Kusama, SystemChain::Encointer) => {
                let slot: Option<encointer_kusama::runtime_types::sp_consensus_slots::Slot> =
                    api.storage().at(at).fetch(&encointer_kusama::storage().aura().current_slot()).await?;
                let auths: Option<
                    encointer_kusama::runtime_types::bounded_collections::bounded_vec::BoundedVec<
                        encointer_kusama::runtime_types::sp_consensus_aura::sr25519::app_sr25519::Public
                    >
                > = api.storage().at(at).fetch(&encointer_kusama::storage().aura().authorities()).await?;
                pick_key!(slot, auths)
            }
            _ => None,
        };

        Ok(key_opt)
    }

    /// Look up the account that owns a session key
    async fn session_key_owner_typed(
        &self,
        api: &OnlineClient<PolkadotConfig>,
        at: H256,
        network: Network,
        chain: SystemChain,
        session_key_raw32: [u8; 32],
    ) -> anyhow::Result<Option<[u8; 32]>> {
        let aura = *b"aura";

        macro_rules! fetch_owner {
            ($call:expr) => {{
                let owner_opt = api.storage().at(at).fetch(&$call).await?;
                Ok(owner_opt.map(|acc| account_to_raw32(acc)))
            }};
        }

        match (network, chain) {
            // Polkadot chains
            (Network::Polkadot, SystemChain::AssetHub) => {
                let kt = asset_hub_polkadot::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = asset_hub_polkadot::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            (Network::Polkadot, SystemChain::BridgeHub) => {
                let kt = bridge_hub_polkadot::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = bridge_hub_polkadot::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            (Network::Polkadot, SystemChain::Collectives) => {
                let kt = collectives_polkadot::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = collectives_polkadot::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            (Network::Polkadot, SystemChain::Coretime) => {
                let kt = coretime_polkadot::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = coretime_polkadot::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            (Network::Polkadot, SystemChain::People) => {
                let kt = people_polkadot::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = people_polkadot::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            // Kusama chains
            (Network::Kusama, SystemChain::AssetHub) => {
                let kt = asset_hub_kusama::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = asset_hub_kusama::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            (Network::Kusama, SystemChain::BridgeHub) => {
                let kt = bridge_hub_kusama::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = bridge_hub_kusama::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            (Network::Kusama, SystemChain::Coretime) => {
                let kt = coretime_kusama::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = coretime_kusama::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            (Network::Kusama, SystemChain::People) => {
                let kt = people_kusama::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = people_kusama::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            (Network::Kusama, SystemChain::Encointer) => {
                let kt = encointer_kusama::runtime_types::sp_core::crypto::KeyTypeId(aura);
                let call = encointer_kusama::storage().session().key_owner((kt, session_key_raw32.to_vec()));
                fetch_owner!(call)
            }
            _ => Ok(None),
        }
    }
}

impl Default for BlockTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse SS58 address to raw 32-byte account ID
fn parse_ss58_to_raw32(address: &str) -> anyhow::Result<[u8; 32]> {
    use sp_core::crypto::Ss58Codec;
    use sp_core::sr25519::Public;
    
    let public = Public::from_ss58check(address)
        .map_err(|e| anyhow::anyhow!("Invalid SS58 address: {:?}", e))?;
    
    Ok(public.0)
}

/// Convert a runtime AccountId32 to raw [u8; 32] by SCALE-encoding
fn account_to_raw32<T: Encode>(acc: T) -> [u8; 32] {
    let bytes = acc.encode();
    let mut out = [0u8; 32];
    if bytes.len() >= 32 {
        out.copy_from_slice(&bytes[..32]);
    }
    out
}
