//! Chain client for interacting with Polkadot system chains.
//!
//! Uses subxt's dynamic API to work with any chain without compile-time metadata.

use anyhow::{Context, Result};
use subxt::dynamic::Value;
use subxt::utils::AccountId32;
use subxt::{OnlineClient, PolkadotConfig};
use tracing::{debug, info};

use crate::config::{Network, SystemChain};
use crate::error::CollatorError;

/// Candidate information from the collator selection pallet
#[derive(Debug, Clone)]
pub struct CandidateInfo {
    pub who: AccountId32,
    pub deposit: u128,
}

/// Collator status on a chain
#[derive(Debug, Clone, PartialEq)]
pub enum CollatorStatus {
    /// Account is in the invulnerables list
    Invulnerable,
    /// Account is a registered candidate with the given deposit
    Candidate { deposit: u128 },
    /// Account is not a collator
    NotCollator,
}

/// Client for interacting with a system chain
pub struct ChainClient {
    api: OnlineClient<PolkadotConfig>,
    network: Network,
    #[allow(dead_code)]
    chain: SystemChain,
    chain_name: String,
}

impl ChainClient {
    /// Connect to a chain
    pub async fn connect(rpc_url: &str, network: Network, chain: SystemChain) -> Result<Self> {
        info!("Connecting to {} at {}", chain.display_name(network), rpc_url);

        let api = OnlineClient::<PolkadotConfig>::from_url(rpc_url)
            .await
            .context("Failed to connect to chain")?;

        info!("Connected successfully to {}", chain.display_name(network));

        Ok(Self {
            api,
            network,
            chain,
            chain_name: chain.display_name(network),
        })
    }

    /// Get the chain name for display/logging
    pub fn chain_name(&self) -> &str {
        &self.chain_name
    }

    /// Get the network
    pub fn network(&self) -> Network {
        self.network
    }

    /// Parse an SS58 address to AccountId32
    pub fn parse_address(&self, address: &str) -> Result<AccountId32> {
        address
            .parse::<AccountId32>()
            .map_err(|e| CollatorError::InvalidAddress(format!("{}: {}", address, e)).into())
    }

    /// Check the collator status for an account
    pub async fn get_collator_status(&self, account: &AccountId32) -> Result<CollatorStatus> {
        // First check invulnerables
        let invulnerables = self.get_invulnerables().await?;
        if invulnerables.contains(account) {
            debug!("{} is an invulnerable on {}", account, self.chain_name);
            return Ok(CollatorStatus::Invulnerable);
        }

        // Then check candidates
        let candidates = self.get_candidates().await?;
        if let Some(candidate) = candidates.iter().find(|c| &c.who == account) {
            debug!(
                "{} is a candidate on {} with deposit {}",
                account, self.chain_name, candidate.deposit
            );
            return Ok(CollatorStatus::Candidate {
                deposit: candidate.deposit,
            });
        }

        debug!("{} is not a collator on {}", account, self.chain_name);
        Ok(CollatorStatus::NotCollator)
    }

    /// Get the list of invulnerable collators
    pub async fn get_invulnerables(&self) -> Result<Vec<AccountId32>> {
        let storage_query = subxt::dynamic::storage("CollatorSelection", "Invulnerables", ());

        let result = self
            .api
            .storage()
            .at_latest()
            .await?
            .fetch(&storage_query)
            .await?;

        match result {
            Some(value) => {
                let decoded = value.to_value()?;
                parse_account_list(&decoded)
            }
            None => Ok(vec![]),
        }
    }

    /// Get the list of candidate collators with their deposits
    pub async fn get_candidates(&self) -> Result<Vec<CandidateInfo>> {
        let storage_query = subxt::dynamic::storage("CollatorSelection", "CandidateList", ());

        let result = self
            .api
            .storage()
            .at_latest()
            .await?
            .fetch(&storage_query)
            .await?;

        match result {
            Some(value) => {
                let decoded = value.to_value()?;
                parse_candidate_list(&decoded)
            }
            None => Ok(vec![]),
        }
    }

    /// Get the candidacy bond amount
    pub async fn get_candidacy_bond(&self) -> Result<u128> {
        let storage_query = subxt::dynamic::storage("CollatorSelection", "CandidacyBond", ());

        let result = self
            .api
            .storage()
            .at_latest()
            .await?
            .fetch(&storage_query)
            .await?
            .context("CandidacyBond not found")?;

        let decoded = result.to_value()?;
        parse_u128(&decoded)
    }

    /// Get the desired number of candidates
    #[allow(dead_code)]
    pub async fn get_desired_candidates(&self) -> Result<u32> {
        let storage_query = subxt::dynamic::storage("CollatorSelection", "DesiredCandidates", ());

        let result = self
            .api
            .storage()
            .at_latest()
            .await?
            .fetch(&storage_query)
            .await?
            .context("DesiredCandidates not found")?;

        let decoded = result.to_value()?;
        parse_u32(&decoded)
    }

    /// Get the free balance of an account
    pub async fn get_free_balance(&self, account: &AccountId32) -> Result<u128> {
        let storage_query = subxt::dynamic::storage(
            "System",
            "Account",
            vec![Value::from_bytes(account.0)],
        );

        let result = self
            .api
            .storage()
            .at_latest()
            .await?
            .fetch(&storage_query)
            .await?;

        match result {
            Some(value) => {
                let decoded = value.to_value()?;
                parse_free_balance(&decoded)
            }
            None => Ok(0),
        }
    }

    /// Get the minimum deposit among current candidates (for determining competitive bond)
    #[allow(dead_code)]
    pub async fn get_minimum_candidate_deposit(&self) -> Result<Option<u128>> {
        let candidates = self.get_candidates().await?;
        Ok(candidates.iter().map(|c| c.deposit).min())
    }

    /// Register as a collator candidate via proxy
    ///
    /// Returns the transaction hash on success
    pub async fn register_as_candidate_via_proxy(
        &self,
        collator_account: &AccountId32,
        proxy_signer: &subxt_signer::sr25519::Keypair,
    ) -> Result<String> {
        info!(
            "Registering {} as candidate on {} via proxy",
            collator_account, self.chain_name
        );

        // Build the inner call: collatorSelection.registerAsCandidate()
        let inner_call = subxt::dynamic::tx("CollatorSelection", "register_as_candidate", Vec::<Value>::new());

        // Wrap it in a proxy call using NonTransfer proxy type
        // proxy.proxy(real, force_proxy_type, call)
        let proxy_call = subxt::dynamic::tx(
            "Proxy",
            "proxy",
            vec![
                // real: the account being proxied (the collator)
                Value::unnamed_variant("Id", [Value::from_bytes(collator_account.0)]),
                // force_proxy_type: Some(NonTransfer) - use NonTransfer proxy
                Value::unnamed_variant("Some", [Value::unnamed_variant("NonTransfer", [])]),
                // call: the inner call
                inner_call.into_value(),
            ],
        );

        let tx_progress = self
            .api
            .tx()
            .sign_and_submit_then_watch_default(&proxy_call, proxy_signer)
            .await
            .context("Failed to submit proxy transaction")?;

        let events = tx_progress
            .wait_for_finalized_success()
            .await
            .context("Transaction failed")?;

        let tx_hash = format!("{:?}", events.extrinsic_hash());
        info!(
            "Successfully registered {} as candidate on {} (tx: {})",
            collator_account, self.chain_name, tx_hash
        );

        Ok(tx_hash)
    }

    /// Update (increase) the candidacy bond via proxy
    pub async fn update_bond_via_proxy(
        &self,
        collator_account: &AccountId32,
        proxy_signer: &subxt_signer::sr25519::Keypair,
        new_bond: u128,
    ) -> Result<String> {
        info!(
            "Updating bond for {} to {} on {} via proxy",
            collator_account, new_bond, self.chain_name
        );

        // Build the inner call: collatorSelection.updateBond(new_deposit)
        let inner_call = subxt::dynamic::tx(
            "CollatorSelection",
            "update_bond",
            vec![Value::u128(new_bond)],
        );

        // Wrap it in a proxy call using NonTransfer proxy type
        let proxy_call = subxt::dynamic::tx(
            "Proxy",
            "proxy",
            vec![
                Value::unnamed_variant("Id", [Value::from_bytes(collator_account.0)]),
                // force_proxy_type: Some(NonTransfer)
                Value::unnamed_variant("Some", [Value::unnamed_variant("NonTransfer", [])]),
                inner_call.into_value(),
            ],
        );

        let tx_progress = self
            .api
            .tx()
            .sign_and_submit_then_watch_default(&proxy_call, proxy_signer)
            .await
            .context("Failed to submit proxy transaction")?;

        let events = tx_progress
            .wait_for_finalized_success()
            .await
            .context("Transaction failed")?;

        let tx_hash = format!("{:?}", events.extrinsic_hash());
        info!(
            "Successfully updated bond for {} to {} on {} (tx: {})",
            collator_account, new_bond, self.chain_name, tx_hash
        );

        Ok(tx_hash)
    }

    /// Take a candidate slot (replacing an existing candidate with lower bond) via proxy
    #[allow(dead_code)]
    pub async fn take_candidate_slot_via_proxy(
        &self,
        collator_account: &AccountId32,
        proxy_signer: &subxt_signer::sr25519::Keypair,
        deposit: u128,
        target: &AccountId32,
    ) -> Result<String> {
        info!(
            "Taking candidate slot from {} with deposit {} on {} via proxy",
            target, deposit, self.chain_name
        );

        // Build the inner call: collatorSelection.takeCandidateSlot(deposit, target)
        let inner_call = subxt::dynamic::tx(
            "CollatorSelection",
            "take_candidate_slot",
            vec![
                Value::u128(deposit),
                Value::from_bytes(target.0),
            ],
        );

        // Wrap it in a proxy call using NonTransfer proxy type
        let proxy_call = subxt::dynamic::tx(
            "Proxy",
            "proxy",
            vec![
                Value::unnamed_variant("Id", [Value::from_bytes(collator_account.0)]),
                // force_proxy_type: Some(NonTransfer)
                Value::unnamed_variant("Some", [Value::unnamed_variant("NonTransfer", [])]),
                inner_call.into_value(),
            ],
        );

        let tx_progress = self
            .api
            .tx()
            .sign_and_submit_then_watch_default(&proxy_call, proxy_signer)
            .await
            .context("Failed to submit proxy transaction")?;

        let events = tx_progress
            .wait_for_finalized_success()
            .await
            .context("Transaction failed")?;

        let tx_hash = format!("{:?}", events.extrinsic_hash());
        info!(
            "Successfully took candidate slot on {} (tx: {})",
            self.chain_name, tx_hash
        );

        Ok(tx_hash)
    }
}

// Helper functions to parse dynamic values
use subxt::ext::scale_value::{Value as ScaleValue, ValueDef, Composite, Primitive};

fn parse_account_list<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<Vec<AccountId32>> {
    let mut accounts = Vec::new();

    if let ValueDef::Composite(composite) = &value.value {
        match composite {
            Composite::Unnamed(items) => {
                for item in items {
                    if let Ok(account) = parse_account_id(item) {
                        accounts.push(account);
                    }
                }
            }
            Composite::Named(items) => {
                for (_, item) in items {
                    if let Ok(account) = parse_account_id(item) {
                        accounts.push(account);
                    }
                }
            }
        }
    }

    Ok(accounts)
}

fn parse_candidate_list<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<Vec<CandidateInfo>> {
    let mut candidates = Vec::new();

    if let ValueDef::Composite(composite) = &value.value {
        match composite {
            Composite::Unnamed(items) => {
                for item in items {
                    if let Ok(candidate) = parse_candidate_info(item) {
                        candidates.push(candidate);
                    }
                }
            }
            Composite::Named(items) => {
                for (_, item) in items {
                    if let Ok(candidate) = parse_candidate_info(item) {
                        candidates.push(candidate);
                    }
                }
            }
        }
    }

    Ok(candidates)
}

fn parse_candidate_info<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<CandidateInfo> {
    // CandidateInfo { who: AccountId32, deposit: u128 }
    if let ValueDef::Composite(Composite::Named(fields)) = &value.value {
        let mut who = None;
        let mut deposit = None;

        for (name, val) in fields {
            match name.as_str() {
                "who" => who = Some(parse_account_id(val)?),
                "deposit" => deposit = Some(parse_u128(val)?),
                _ => {}
            }
        }

        if let (Some(who), Some(deposit)) = (who, deposit) {
            return Ok(CandidateInfo { who, deposit });
        }
    }

    Err(anyhow::anyhow!("Failed to parse CandidateInfo"))
}

fn parse_account_id<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<AccountId32> {
    // Try to extract bytes from various representations
    if let ValueDef::Composite(composite) = &value.value {
        match composite {
            Composite::Unnamed(items) if items.len() == 1 => {
                return parse_account_id(&items[0]);
            }
            Composite::Unnamed(items) if items.len() == 32 => {
                let mut bytes = [0u8; 32];
                for (i, item) in items.iter().enumerate() {
                    if let ValueDef::Primitive(Primitive::U128(n)) = &item.value {
                        bytes[i] = *n as u8;
                    }
                }
                return Ok(AccountId32(bytes));
            }
            _ => {}
        }
    }

    // Try primitive u256 (sometimes used for account ids in some contexts)
    if let ValueDef::Primitive(prim) = &value.value {
        if let Primitive::U256(bytes) = prim {
            let mut account_bytes = [0u8; 32];
            account_bytes.copy_from_slice(&bytes[..32]);
            return Ok(AccountId32(account_bytes));
        }
    }

    Err(anyhow::anyhow!("Failed to parse AccountId32"))
}

fn parse_u128<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<u128> {
    match &value.value {
        ValueDef::Primitive(Primitive::U128(n)) => Ok(*n),
        _ => Err(anyhow::anyhow!("Failed to parse u128: {:?}", value)),
    }
}

fn parse_u32<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<u32> {
    match &value.value {
        ValueDef::Primitive(Primitive::U128(n)) => Ok(*n as u32),
        _ => Err(anyhow::anyhow!("Failed to parse u32: {:?}", value)),
    }
}

fn parse_free_balance<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<u128> {
    // AccountInfo { nonce, consumers, providers, sufficients, data: AccountData { free, reserved, frozen, flags } }
    if let ValueDef::Composite(Composite::Named(fields)) = &value.value {
        for (name, val) in fields {
            if name == "data" {
                if let ValueDef::Composite(Composite::Named(data_fields)) = &val.value {
                    for (data_name, data_val) in data_fields {
                        if data_name == "free" {
                            return parse_u128(data_val);
                        }
                    }
                }
            }
        }
    }

    Err(anyhow::anyhow!("Failed to parse free balance"))
}
