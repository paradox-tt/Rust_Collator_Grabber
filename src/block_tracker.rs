//! Background block tracker for monitoring last authored blocks per chain.
//!
//! This module runs background tasks that monitor each chain and track when
//! the collator last authored a block.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::chain_client::ChainClient;
use crate::config::{default_rpc_url, AppConfig, Network, SystemChain};

/// Tracks last authored block times for all chains
#[derive(Debug, Clone)]
pub struct LastBlockInfo {
    /// Time since the collator last authored a block (if known)
    pub time_ago: Option<Duration>,
    /// When this information was last updated
    pub last_updated: Instant,
    /// Whether the tracker is currently running for this chain
    pub is_tracking: bool,
}

impl Default for LastBlockInfo {
    fn default() -> Self {
        Self {
            time_ago: None,
            last_updated: Instant::now(),
            is_tracking: false,
        }
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

    /// Get all chain names being tracked
    pub async fn get_tracked_chains(&self) -> Vec<String> {
        let data = self.data.read().await;
        data.keys().cloned().collect()
    }

    /// Update the last block info for a chain
    async fn update_chain(&self, chain_name: &str, time_ago: Option<Duration>) {
        let mut data = self.data.write().await;
        data.insert(chain_name.to_string(), LastBlockInfo {
            time_ago,
            last_updated: Instant::now(),
            is_tracking: true,
        });
    }

    /// Mark a chain as having tracking issues
    async fn mark_tracking_error(&self, chain_name: &str) {
        let mut data = self.data.write().await;
        if let Some(info) = data.get_mut(chain_name) {
            info.last_updated = Instant::now();
            // Keep the old time_ago value, just update the timestamp
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
            let handle = self.clone().spawn_chain_tracker(
                Network::Polkadot,
                chain,
                config.clone(),
            );
            handles.push(handle);
        }

        // Start Kusama chain trackers
        for chain in kusama_chains {
            let handle = self.clone().spawn_chain_tracker(
                Network::Kusama,
                chain,
                config.clone(),
            );
            handles.push(handle);
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
        if !chain.valid_networks().contains(&network) {
            return;
        }

        let chain_name = chain.display_name(network);
        let collator_address = config.collator_address(network);
        let rpc_url = config
            .chain_config(network, chain)
            .map(|c| c.rpc_url.clone())
            .unwrap_or_else(|| default_rpc_url(network, chain).to_string());

        info!("Starting block tracker for {}", chain_name);

        // Initialize tracking entry
        {
            let mut data = self.data.write().await;
            data.insert(chain_name.clone(), LastBlockInfo {
                time_ago: None,
                last_updated: Instant::now(),
                is_tracking: true,
            });
        }

        // Main tracking loop - check every 30 seconds
        let check_interval = Duration::from_secs(30);
        
        loop {
            if self.is_shutdown().await {
                info!("Block tracker for {} shutting down", chain_name);
                break;
            }

            match self.check_last_block(&chain_name, &rpc_url, network, chain, collator_address).await {
                Ok(time_ago) => {
                    self.update_chain(&chain_name, time_ago).await;
                }
                Err(e) => {
                    debug!("Error checking last block for {}: {}", chain_name, e);
                    self.mark_tracking_error(&chain_name).await;
                }
            }

            tokio::time::sleep(check_interval).await;
        }
    }

    /// Check the last authored block for a chain
    async fn check_last_block(
        &self,
        chain_name: &str,
        rpc_url: &str,
        network: Network,
        chain: SystemChain,
        collator_address: &str,
    ) -> anyhow::Result<Option<Duration>> {
        let client = ChainClient::connect(rpc_url, network, chain).await?;
        let collator_account = client.parse_address(collator_address)?;
        
        client.get_last_authored_block_time(&collator_account).await
    }
}

impl Default for BlockTracker {
    fn default() -> Self {
        Self::new()
    }
}
