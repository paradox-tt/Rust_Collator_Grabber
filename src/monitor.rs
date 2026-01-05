//! Monitoring logic for collator status and automatic re-registration.

use anyhow::{Context, Result};
use subxt::utils::AccountId32;
use tracing::{debug, error, info, warn};

use crate::chain_client::{ChainClient, CollatorStatus};
use crate::config::{chain_supports_proxy, default_rpc_url, AppConfig, Network, SystemChain};
use crate::slack::SlackNotifier;

/// Result of monitoring a single chain
#[derive(Debug)]
pub struct MonitorResult {
    pub chain_name: String,
    pub status: MonitorStatus,
}

#[derive(Debug)]
pub enum MonitorStatus {
    /// Already a collator (invulnerable or candidate)
    AlreadyCollator(CollatorStatus),
    /// Successfully registered as candidate
    RegisteredAsCandidate { bond: u128, tx_hash: String },
    /// Successfully updated bond to higher amount
    UpdatedBond { old_bond: u128, new_bond: u128, tx_hash: String },
    /// Could not register due to insufficient funds
    InsufficientFunds { available: u128, required: u128 },
    /// Error occurred during monitoring
    Error(String),
    /// Chain was skipped (not enabled or not valid for network)
    Skipped(String),
}

/// Monitor and manage collator status across all chains
pub struct CollatorMonitor {
    config: AppConfig,
    proxy_signer: subxt_signer::sr25519::Keypair,
    slack: SlackNotifier,
}

impl CollatorMonitor {
    /// Create a new collator monitor
    pub fn new(config: AppConfig) -> Result<Self> {
        // Parse the proxy seed to create a signer
        let proxy_signer = parse_seed(&config.proxy_seed)
            .context("Failed to parse proxy seed")?;

        let slack = SlackNotifier::new(config.slack_webhook_url.clone());

        Ok(Self {
            config,
            proxy_signer,
            slack,
        })
    }

    /// Run monitoring for all configured chains
    pub async fn monitor_all_chains(&self) -> Vec<MonitorResult> {
        let mut results = Vec::new();

        // Define all chain/network combinations
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

        // Monitor Polkadot chains
        for chain in polkadot_chains {
            let result = self.monitor_chain(Network::Polkadot, chain).await;
            results.push(result);
        }

        // Monitor Kusama chains
        for chain in kusama_chains {
            let result = self.monitor_chain(Network::Kusama, chain).await;
            results.push(result);
        }

        results
    }

    /// Monitor a single chain
    pub async fn monitor_chain(&self, network: Network, chain: SystemChain) -> MonitorResult {
        let chain_name = chain.display_name(network);

        // Check if chain is valid for this network
        if !chain.valid_networks().contains(&network) {
            return MonitorResult {
                chain_name,
                status: MonitorStatus::Skipped(format!(
                    "Chain not available on {:?}",
                    network
                )),
            };
        }

        // Check if chain supports proxy accounts (BridgeHub doesn't)
        if !chain_supports_proxy(chain) {
            return MonitorResult {
                chain_name,
                status: MonitorStatus::Skipped(
                    "Chain does not support proxy accounts for collator registration".to_string()
                ),
            };
        }

        // Check if chain is enabled in config
        if let Some(chain_config) = self.config.chain_config(network, chain) {
            if !chain_config.enabled {
                return MonitorResult {
                    chain_name,
                    status: MonitorStatus::Skipped("Chain disabled in config".to_string()),
                };
            }
        }

        // Get RPC URL
        let rpc_url = self
            .config
            .chain_config(network, chain)
            .map(|c| c.rpc_url.as_str())
            .unwrap_or_else(|| default_rpc_url(network, chain));

        // Get collator address for this network
        let collator_address = self.config.collator_address(network);

        info!("Monitoring {} for collator {}", chain_name, collator_address);

        match self
            .monitor_chain_internal(network, chain, rpc_url, collator_address)
            .await
        {
            Ok(status) => MonitorResult { chain_name, status },
            Err(e) => {
                error!("Error monitoring {}: {}", chain_name, e);
                let _ = self.slack.notify_error(&chain_name, &e.to_string()).await;
                MonitorResult {
                    chain_name,
                    status: MonitorStatus::Error(e.to_string()),
                }
            }
        }
    }

    async fn monitor_chain_internal(
        &self,
        network: Network,
        chain: SystemChain,
        rpc_url: &str,
        collator_address: &str,
    ) -> Result<MonitorStatus> {
        // Connect to chain
        let client = ChainClient::connect(rpc_url, network, chain).await?;

        // Parse collator address
        let collator_account = client.parse_address(collator_address)?;

        // Check current collator status
        let status = client.get_collator_status(&collator_account).await?;

        // Get balance info for bond calculations
        let free_balance = client.get_free_balance(&collator_account).await?;
        let reserve_amount = network.reserve_amount();
        let available_for_bond = free_balance.saturating_sub(reserve_amount);
        let candidacy_bond = client.get_candidacy_bond().await?;

        match status {
            CollatorStatus::Invulnerable => {
                info!(
                    "{} is an invulnerable collator on {}",
                    collator_address,
                    client.chain_name()
                );
                Ok(MonitorStatus::AlreadyCollator(CollatorStatus::Invulnerable))
            }
            CollatorStatus::Candidate { deposit: current_bond } => {
                info!(
                    "{} is already a candidate on {} with deposit {}",
                    collator_address,
                    client.chain_name(),
                    current_bond
                );
                
                // Check if we should increase the bond
                if available_for_bond > current_bond {
                    info!(
                        "Increasing bond from {} to {} on {}",
                        current_bond, available_for_bond, client.chain_name()
                    );
                    
                    let tx_hash = client
                        .update_bond_via_proxy(&collator_account, &self.proxy_signer, available_for_bond)
                        .await?;

                    let _ = self
                        .slack
                        .notify_bond_update(
                            client.chain_name(),
                            &collator_account.to_string(),
                            current_bond,
                            available_for_bond,
                            network.symbol(),
                            network.decimals(),
                        )
                        .await;

                    Ok(MonitorStatus::UpdatedBond {
                        old_bond: current_bond,
                        new_bond: available_for_bond,
                        tx_hash,
                    })
                } else {
                    Ok(MonitorStatus::AlreadyCollator(CollatorStatus::Candidate {
                        deposit: current_bond,
                    }))
                }
            }
            CollatorStatus::NotCollator => {
                info!(
                    "{} is NOT a collator on {}, attempting registration",
                    collator_address,
                    client.chain_name()
                );
                self.attempt_registration(&client, &collator_account, network, available_for_bond, candidacy_bond)
                    .await
            }
        }
    }

    async fn attempt_registration(
        &self,
        client: &ChainClient,
        collator_account: &AccountId32,
        network: Network,
        available_for_bond: u128,
        candidacy_bond: u128,
    ) -> Result<MonitorStatus> {
        debug!("Candidacy bond: {}", candidacy_bond);
        debug!("Available for bond: {}", available_for_bond);

        // Check if we have enough funds to register
        if available_for_bond < candidacy_bond {
            warn!(
                "Cannot register on {}: available {} < bond requirement {}",
                client.chain_name(),
                available_for_bond,
                candidacy_bond
            );

            let _ = self
                .slack
                .alert_insufficient_funds(
                    client.chain_name(),
                    &collator_account.to_string(),
                    available_for_bond,
                    candidacy_bond,
                    network.symbol(),
                    network.decimals(),
                )
                .await;

            return Ok(MonitorStatus::InsufficientFunds {
                available: available_for_bond,
                required: candidacy_bond,
            });
        }

        // Register as candidate
        let tx_hash = client
            .register_as_candidate_via_proxy(collator_account, &self.proxy_signer)
            .await?;

        // After registration, update the bond to use maximum available funds
        if available_for_bond > candidacy_bond {
            info!(
                "Updating bond from {} to {} on {}",
                candidacy_bond, available_for_bond, client.chain_name()
            );
            match client
                .update_bond_via_proxy(collator_account, &self.proxy_signer, available_for_bond)
                .await
            {
                Ok(_) => {
                    info!("Successfully increased bond to maximum");
                }
                Err(e) => {
                    warn!("Failed to increase bond after registration: {}", e);
                    // Don't fail the whole operation, registration was successful
                }
            }
        }

        let _ = self
            .slack
            .notify_registration_success(
                client.chain_name(),
                &collator_account.to_string(),
                available_for_bond,
                network.symbol(),
                network.decimals(),
            )
            .await;

        Ok(MonitorStatus::RegisteredAsCandidate {
            bond: available_for_bond,
            tx_hash,
        })
    }
}

/// Parse a seed phrase or hex seed into a keypair
fn parse_seed(seed: &str) -> Result<subxt_signer::sr25519::Keypair> {
    use subxt_signer::SecretUri;
    use std::str::FromStr;

    let seed = seed.trim();

    // Try as mnemonic first (contains spaces)
    if seed.contains(' ') {
        // Parse mnemonic using bip39
        let mnemonic = bip39::Mnemonic::parse(seed)
            .map_err(|e| anyhow::anyhow!("Invalid mnemonic: {}", e))?;
        
        subxt_signer::sr25519::Keypair::from_phrase(&mnemonic, None)
            .map_err(|e| anyhow::anyhow!("Failed to create keypair from mnemonic: {}", e))
    } else if seed.starts_with("0x") {
        // It's a hex seed - convert to secret key bytes
        let bytes = hex::decode(&seed[2..])
            .context("Invalid hex seed")?;
        
        if bytes.len() != 32 {
            return Err(anyhow::anyhow!("Hex seed must be 32 bytes, got {}", bytes.len()));
        }

        let mut seed_bytes = [0u8; 32];
        seed_bytes.copy_from_slice(&bytes);
        
        subxt_signer::sr25519::Keypair::from_secret_key(seed_bytes)
            .map_err(|e| anyhow::anyhow!("Invalid seed: {}", e))
    } else {
        // Try as URI (e.g., "//Alice" or other derivation paths)
        let uri = SecretUri::from_str(seed)
            .map_err(|e| anyhow::anyhow!("Invalid URI format: {}", e))?;
        
        subxt_signer::sr25519::Keypair::from_uri(&uri)
            .map_err(|e| anyhow::anyhow!("Failed to create keypair from URI: {}", e))
    }
}
