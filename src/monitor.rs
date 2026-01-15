//! Monitoring logic for collator status and automatic re-registration.

use std::sync::Arc;
use anyhow::{Context, Result};
use subxt::utils::AccountId32;
use tracing::{debug, error, info, warn};

use crate::block_tracker::BlockTracker;
use crate::chain_client::{ChainClient, CollatorStatus};
use crate::config::{chain_supports_proxy, default_rpc_url, AppConfig, Network, SystemChain};
use crate::slack::{SlackNotifier, ChainSlotInfo};

/// Format a balance with proper decimal places and symbol
fn format_balance(balance: u128, decimals: u32, symbol: &str) -> String {
    let divisor = 10u128.pow(decimals);
    let whole = balance / divisor;
    let fraction = balance % divisor;

    if fraction == 0 {
        format!("{} {}", whole, symbol)
    } else {
        let fraction_str = format!("{:0>width$}", fraction, width = decimals as usize);
        let trimmed = fraction_str.trim_end_matches('0');
        let display_decimals = trimmed.len().min(4);
        format!("{}.{} {}", whole, &fraction_str[..display_decimals], symbol)
    }
}

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
    /// Could not register due to insufficient funds for minimum bond
    InsufficientFunds { available: u128, required: u128 },
    /// Could not compete - bond too low to beat lowest candidate
    CannotCompete { available: u128, lowest_candidate: u128, needed: u128 },
    /// Manual action required (chain doesn't support proxy or is disabled)
    ManualActionRequired { reason: String, current_status: CollatorStatus },
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
    block_tracker: Arc<BlockTracker>,
}

impl CollatorMonitor {
    /// Create a new collator monitor
    pub fn new(config: AppConfig, block_tracker: Arc<BlockTracker>) -> Result<Self> {
        // Parse the proxy seed to create a signer
        let proxy_signer = parse_seed(&config.proxy_seed)
            .context("Failed to parse proxy seed")?;

        let slack = SlackNotifier::new(
            config.slack_webhook_url.clone(),
            config.slack_user_ids.clone(),
        );

        Ok(Self {
            config,
            proxy_signer,
            slack,
            block_tracker,
        })
    }

    /// Get reference to slack notifier (for summary sending)
    pub fn slack(&self) -> &SlackNotifier {
        &self.slack
    }

    /// Get reference to block tracker
    pub fn block_tracker(&self) -> &Arc<BlockTracker> {
        &self.block_tracker
    }

    /// Get the summary interval from config
    pub fn summary_interval_secs(&self) -> u64 {
        self.config.summary_interval_secs
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

        // Check if chain is explicitly disabled in config
        let chain_enabled = self.config.chain_config(network, chain)
            .map(|c| c.enabled)
            .unwrap_or(true); // Default to enabled if not specified

        // Check if chain supports proxy accounts (BridgeHub doesn't)
        let supports_proxy = chain_supports_proxy(chain);
        
        // Determine if this is a read-only check (no automatic actions)
        let read_only = !supports_proxy || !chain_enabled;

        // Get RPC URL
        let rpc_url = self
            .config
            .chain_config(network, chain)
            .map(|c| c.rpc_url.as_str())
            .unwrap_or_else(|| default_rpc_url(network, chain));

        // Get collator address for this network
        let collator_address = self.config.collator_address(network);

        info!("Monitoring {} for collator {} (read_only: {})", chain_name, collator_address, read_only);

        match self
            .monitor_chain_internal(network, chain, rpc_url, collator_address, read_only)
            .await
        {
            Ok(status) => {
                // Check if this chain had an outstanding issue that's now resolved
                let had_issue = self.slack.has_outstanding_issue(&chain_name);
                let was_manual = self.slack.was_manual_action_required(&chain_name);
                
                // If chain is now healthy (AlreadyCollator) and had an issue, notify resolution
                if had_issue {
                    if let MonitorStatus::AlreadyCollator(ref collator_status) = status {
                        let status_str = match collator_status {
                            CollatorStatus::Invulnerable => "Invulnerable".to_string(),
                            CollatorStatus::Candidate { deposit } => {
                                format!("Candidate (bond: {})", 
                                    format_balance(*deposit, network.decimals(), network.symbol()))
                            }
                            CollatorStatus::NotCollator => "Not a collator".to_string(),
                        };
                        let _ = self.slack.notify_issue_resolved(&chain_name, was_manual, &status_str).await;
                    }
                }
                
                MonitorResult { chain_name, status }
            }
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

    /// Collect slot information for all chains (for summary)
    pub async fn collect_slot_info(&self) -> Vec<ChainSlotInfo> {
        let mut slots = Vec::new();

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

        // Collect Polkadot chain slots
        for chain in polkadot_chains {
            if let Some(info) = self.get_chain_slot_info(Network::Polkadot, chain).await {
                slots.push(info);
            }
        }

        // Collect Kusama chain slots
        for chain in kusama_chains {
            if let Some(info) = self.get_chain_slot_info(Network::Kusama, chain).await {
                slots.push(info);
            }
        }

        slots
    }

    /// Get slot info for a single chain
    async fn get_chain_slot_info(&self, network: Network, chain: SystemChain) -> Option<ChainSlotInfo> {
        if !chain.valid_networks().contains(&network) {
            return None;
        }

        let chain_name = chain.display_name(network);
        let rpc_url = self
            .config
            .chain_config(network, chain)
            .map(|c| c.rpc_url.as_str())
            .unwrap_or_else(|| default_rpc_url(network, chain));

        let collator_address = self.config.collator_address(network);

        let client = match ChainClient::connect(rpc_url, network, chain).await {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to connect to {} for slot info: {}", chain_name, e);
                return None;
            }
        };

        let collator_account = match client.parse_address(collator_address) {
            Ok(a) => a,
            Err(_) => return None,
        };

        let status = match client.get_collator_status(&collator_account).await {
            Ok(s) => s,
            Err(_) => return None,
        };

        let candidates = client.get_candidates().await.unwrap_or_default();
        let max_candidates = client.get_desired_candidates().await.ok();

        let (is_invulnerable, is_candidate, position, your_bond) = match &status {
            CollatorStatus::Invulnerable => (true, false, None, None),
            CollatorStatus::Candidate { deposit } => {
                // Find position in candidate list (sorted by bond descending, so position 1 = highest bond)
                let mut sorted_candidates: Vec<_> = candidates.iter()
                    .filter(|c| c.deposit > 0)
                    .collect();
                sorted_candidates.sort_by(|a, b| b.deposit.cmp(&a.deposit));
                
                let pos = sorted_candidates.iter()
                    .position(|c| c.who == collator_account)
                    .map(|p| p + 1); // 1-indexed
                
                (false, true, pos, Some(*deposit))
            }
            CollatorStatus::NotCollator => (false, false, None, None),
        };

        // Calculate distance from last (lowest bond)
        let lowest_bond = candidates.iter()
            .filter(|c| c.deposit > 0)
            .map(|c| c.deposit)
            .min();
        
        let distance_from_last = match (your_bond, lowest_bond) {
            (Some(your), Some(lowest)) if your > lowest => Some(your - lowest),
            _ => None,
        };

        // Get last authored block time from the block tracker (if we're a collator)
        let last_block_time = if is_invulnerable || is_candidate {
            self.block_tracker.get_last_block(&chain_name).await
                .and_then(|info| info.time_since_last_block())
        } else {
            None
        };

        Some(ChainSlotInfo {
            chain_name,
            is_invulnerable,
            is_candidate,
            position,
            max_candidates,
            total_candidates: candidates.len(),
            your_bond,
            lowest_bond,
            distance_from_last,
            last_block_time,
            token_symbol: network.symbol().to_string(),
            decimals: network.decimals(),
        })
    }

    async fn monitor_chain_internal(
        &self,
        network: Network,
        chain: SystemChain,
        rpc_url: &str,
        collator_address: &str,
        read_only: bool,
    ) -> Result<MonitorStatus> {
        // Connect to chain
        let client = ChainClient::connect(rpc_url, network, chain).await?;

        // Parse collator address
        let collator_account = client.parse_address(collator_address)?;

        // Check current collator status first
        let status = client.get_collator_status(&collator_account).await?;

        // If invulnerable, no action needed - return early
        if status == CollatorStatus::Invulnerable {
            info!(
                "{} is an invulnerable collator on {} - no action needed",
                collator_address,
                client.chain_name()
            );
            return Ok(MonitorStatus::AlreadyCollator(CollatorStatus::Invulnerable));
        }

        // For candidates and non-collators, we need balance and bond info
        let free_balance = client.get_free_balance(&collator_account).await?;
        let reserve_amount = network.reserve_amount();
        let available_for_bond = free_balance.saturating_sub(reserve_amount);
        let candidacy_bond = client.get_candidacy_bond().await?;
        
        // Get current candidates to check competitive bond
        let candidates = client.get_candidates().await?;
        // Get minimum bond from candidates (only those with bond > 0, sorted ascending)
        let lowest_candidate_bond = candidates
            .iter()
            .filter(|c| c.deposit > 0)
            .map(|c| c.deposit)
            .min();

        match status.clone() {
            CollatorStatus::Invulnerable => {
                // Already handled above, but keep for completeness
                unreachable!()
            }
            CollatorStatus::Candidate { deposit: current_bond } => {
                info!(
                    "{} is already a candidate on {} with deposit {}",
                    collator_address,
                    client.chain_name(),
                    current_bond
                );
                
                // When already a candidate, current_bond is LOCKED (not in free_balance)
                // So the new total bond = current_bond + (free_balance - reserve)
                let new_total_bond = current_bond.saturating_add(available_for_bond);
                
                // Log the balance details for debugging
                info!(
                    "{}: free_balance={}, reserve={}, available_to_add={}, current_bond={}, potential_new_bond={}",
                    client.chain_name(),
                    format_balance(free_balance, network.decimals(), network.symbol()),
                    format_balance(reserve_amount, network.decimals(), network.symbol()),
                    format_balance(available_for_bond, network.decimals(), network.symbol()),
                    format_balance(current_bond, network.decimals(), network.symbol()),
                    format_balance(new_total_bond, network.decimals(), network.symbol()),
                );
                
                // Minimum increase threshold (0.1 DOT or 0.01 KSM) to avoid tiny updates
                let min_increase = match network {
                    Network::Polkadot => 1_000_000_000u128, // 0.1 DOT
                    Network::Kusama => 10_000_000_000u128,  // 0.01 KSM
                };
                
                // Check if we have meaningful additional funds to bond
                let should_update = available_for_bond >= min_increase;
                
                if should_update {
                    if read_only {
                        let reason = if !chain_supports_proxy(chain) {
                            "No proxy support - bond update required".to_string()
                        } else {
                            "Chain disabled - bond update required".to_string()
                        };
                        
                        warn!(
                            "Manual action needed on {}: could increase bond from {} to {}",
                            client.chain_name(), 
                            format_balance(current_bond, network.decimals(), network.symbol()),
                            format_balance(new_total_bond, network.decimals(), network.symbol())
                        );
                        
                        // Generate call data for bond update
                        let update_bond_call = client.generate_registration_call_data(new_total_bond);
                        
                        let _ = self
                            .slack
                            .alert_manual_action_required(
                                client.chain_name(),
                                &collator_account.to_string(),
                                &format!("Bond can be increased from {} to {}", 
                                    format_balance(current_bond, network.decimals(), network.symbol()),
                                    format_balance(new_total_bond, network.decimals(), network.symbol())),
                                Some(&update_bond_call),
                            )
                            .await;
                        
                        return Ok(MonitorStatus::ManualActionRequired {
                            reason,
                            current_status: status,
                        });
                    }
                    
                    info!(
                        "Increasing bond from {} to {} on {}",
                        format_balance(current_bond, network.decimals(), network.symbol()),
                        format_balance(new_total_bond, network.decimals(), network.symbol()),
                        client.chain_name()
                    );
                    
                    let tx_hash = client
                        .update_bond_via_proxy(&collator_account, &self.proxy_signer, new_total_bond)
                        .await?;

                    let _ = self
                        .slack
                        .notify_bond_update(
                            client.chain_name(),
                            &collator_account.to_string(),
                            current_bond,
                            new_total_bond,
                            network.symbol(),
                            network.decimals(),
                        )
                        .await;

                    Ok(MonitorStatus::UpdatedBond {
                        old_bond: current_bond,
                        new_bond: new_total_bond,
                        tx_hash,
                    })
                } else {
                    if available_for_bond > 0 {
                        debug!(
                            "{}: Bond increase too small ({} < min {}), skipping",
                            client.chain_name(),
                            format_balance(available_for_bond, network.decimals(), network.symbol()),
                            format_balance(min_increase, network.decimals(), network.symbol())
                        );
                    }
                    Ok(MonitorStatus::AlreadyCollator(CollatorStatus::Candidate {
                        deposit: current_bond,
                    }))
                }
            }
            CollatorStatus::NotCollator => {
                info!(
                    "{} is NOT a collator on {}, checking if we can register",
                    collator_address,
                    client.chain_name()
                );
                
                // First check: do we have enough for minimum candidacy bond?
                if available_for_bond < candidacy_bond {
                    warn!(
                        "Cannot register on {}: available {} < minimum bond requirement {}",
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
                
                // Second check: can we beat the lowest candidate?
                // If there are existing candidates, we need to beat the lowest one
                if let Some(lowest_bond) = lowest_candidate_bond {
                    if available_for_bond <= lowest_bond {
                        let needed = lowest_bond.saturating_sub(available_for_bond) + 1;
                        warn!(
                            "Cannot compete on {}: available {} <= lowest candidate bond {}. Need {} more.",
                            client.chain_name(),
                            available_for_bond,
                            lowest_bond,
                            needed
                        );

                        let _ = self
                            .slack
                            .alert_cannot_compete(
                                client.chain_name(),
                                &collator_account.to_string(),
                                available_for_bond,
                                lowest_bond,
                                needed,
                                network.symbol(),
                                network.decimals(),
                            )
                            .await;

                        return Ok(MonitorStatus::CannotCompete {
                            available: available_for_bond,
                            lowest_candidate: lowest_bond,
                            needed,
                        });
                    }
                }
                
                // We can compete! But check if read_only
                if read_only {
                    let reason = if !chain_supports_proxy(chain) {
                        "No proxy support - registration required".to_string()
                    } else {
                        "Chain disabled - registration required".to_string()
                    };
                    
                    warn!(
                        "Manual action needed on {}: registration required",
                        client.chain_name()
                    );
                    
                    // Generate call data for registration and bond update
                    let register_call = client.generate_register_call_data();
                    let update_bond_call = client.generate_registration_call_data(available_for_bond);
                    
                    let call_info = format!(
                        "1. Register: {}\n2. Update bond: {}",
                        register_call, update_bond_call
                    );
                    
                    let _ = self
                        .slack
                        .alert_manual_action_required(
                            client.chain_name(),
                            &collator_account.to_string(),
                            &format!("Registration required with bond {}", 
                                format_balance(available_for_bond, network.decimals(), network.symbol())),
                            Some(&call_info),
                        )
                        .await;
                    
                    return Ok(MonitorStatus::ManualActionRequired {
                        reason,
                        current_status: status,
                    });
                }
                
                // Try to register
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

        info!(
            "Registering {} as candidate on {} with bond {}",
            collator_account, client.chain_name(), available_for_bond
        );

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
