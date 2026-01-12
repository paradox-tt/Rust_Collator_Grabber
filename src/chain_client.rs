//! Chain client for interacting with Polkadot system chains.
//!
//! Uses subxt's dynamic API to work with any chain without compile-time metadata.

use anyhow::{Context, Result};
use subxt::dynamic::{At, Value};
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
                debug!("Raw Invulnerables data: {:?}", decoded);
                parse_account_list(&decoded)
            }
            None => {
                debug!("Invulnerables storage returned None");
                Ok(vec![])
            }
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
                debug!("Raw CandidateList data: {:?}", decoded);
                parse_candidate_list(&decoded)
            }
            None => {
                debug!("CandidateList storage returned None");
                Ok(vec![])
            }
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

    /// Get the timestamp of the last block authored by this collator
    /// Returns None if no recent block found (searches last ~1000 blocks)
    pub async fn get_last_authored_block_time(
        &self,
        collator_account: &AccountId32,
    ) -> Result<Option<std::time::Duration>> {
        // Get current block
        let current_block = self.api.blocks().at_latest().await?;
        let current_number = current_block.number();
        let current_timestamp = self.get_block_timestamp(&current_block).await?;
        
        // Search backwards through recent blocks (limit to ~1000 blocks = ~3-4 hours on system chains)
        let search_limit = 1000u32;
        let start_block = current_number.saturating_sub(search_limit);
        
        debug!(
            "Searching for blocks authored by {} from block {} to {}",
            collator_account, start_block, current_number
        );
        
        // Get block hashes by querying storage for block hashes
        // We'll iterate by getting blocks relative to the current one
        let mut current_hash = current_block.hash();
        let mut blocks_checked = 0u32;
        
        while blocks_checked < search_limit {
            let block = self.api.blocks().at(current_hash).await?;
            
            // Get the block author from the Aura consensus digest
            if let Some(author) = self.get_block_author(&block).await? {
                if &author == collator_account {
                    // Found a block authored by our collator
                    let block_timestamp = self.get_block_timestamp(&block).await?;
                    let time_ago = std::time::Duration::from_millis(
                        current_timestamp.saturating_sub(block_timestamp)
                    );
                    debug!(
                        "Found block authored by {} ({:?} ago)",
                        collator_account, time_ago
                    );
                    return Ok(Some(time_ago));
                }
            }
            
            // Get parent hash to continue backwards
            let header = block.header();
            if header.number == 0 {
                break; // Reached genesis
            }
            current_hash = header.parent_hash;
            blocks_checked += 1;
        }
        
        debug!("No recent blocks found authored by {}", collator_account);
        Ok(None)
    }

    /// Get the author of a block from the Aura pre-runtime digest
    async fn get_block_author(
        &self,
        block: &subxt::blocks::Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    ) -> Result<Option<AccountId32>> {
        // The author is stored in the Aura pre-runtime digest as a slot number
        // We need to look up which authority was scheduled for that slot
        
        // First, get the authorities list from AuraAuthorities
        let storage_query = subxt::dynamic::storage("Aura", "Authorities", ());
        let authorities = self
            .api
            .storage()
            .at(block.reference())
            .fetch(&storage_query)
            .await?;

        let authorities: Vec<AccountId32> = match authorities {
            Some(value) => {
                let decoded = value.to_value()?;
                parse_aura_authorities(&decoded)?
            }
            None => return Ok(None),
        };

        if authorities.is_empty() {
            return Ok(None);
        }

        // Get the slot from the block header's digest
        let header = block.header();
        for log in header.digest.logs.iter() {
            // Look for PreRuntime digest with Aura engine ID
            if let subxt::config::substrate::DigestItem::PreRuntime(engine_id, data) = log {
                // Aura engine ID is *b"aura"
                if engine_id == b"aura" && data.len() >= 8 {
                    // Slot is encoded as u64 LE
                    let slot = u64::from_le_bytes(data[0..8].try_into().unwrap_or([0u8; 8]));
                    let author_index = (slot as usize) % authorities.len();
                    return Ok(Some(authorities[author_index].clone()));
                }
            }
        }

        Ok(None)
    }

    /// Get the timestamp from a block (from the first extrinsic which is timestamp.set)
    async fn get_block_timestamp(
        &self,
        block: &subxt::blocks::Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    ) -> Result<u64> {
        // The timestamp is set in the first inherent extrinsic: timestamp.set(now)
        let extrinsics = block.extrinsics().await?;
        
        for ext in extrinsics.iter() {
            let pallet = ext.pallet_name()?;
            let call = ext.variant_name()?;
            
            if pallet == "Timestamp" && call == "set" {
                // Decode the timestamp from the call data
                // The timestamp is a Compact<u64>
                let field_values = ext.field_values()?;
                // field_values is a Composite, use At trait to access "now" field
                if let Some(now_value) = field_values.at("now") {
                    // now_value is a &Value, try to get the u128 from it
                    if let Some(ts) = now_value.as_u128() {
                        return Ok(ts as u64);
                    }
                }
            }
        }
        
        // Fallback: return 0 if timestamp not found
        Ok(0)
    }
}

// Helper functions to parse dynamic values
use subxt::ext::scale_value::{Value as ScaleValue, ValueDef, Composite, Primitive};

fn parse_account_list<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<Vec<AccountId32>> {
    let mut accounts = Vec::new();

    match &value.value {
        // The value might be wrapped in a newtype (accessing .0)
        ValueDef::Composite(Composite::Unnamed(items)) => {
            // Check if this is a single-element wrapper or the actual list
            if items.len() == 1 {
                // Could be a newtype wrapper, try to recurse
                if let Ok(inner_accounts) = parse_account_list(&items[0]) {
                    if !inner_accounts.is_empty() {
                        return Ok(inner_accounts);
                    }
                }
                // Otherwise try to parse as a single account
                if let Ok(account) = parse_account_id(&items[0]) {
                    accounts.push(account);
                }
            } else {
                // Multiple items - this is the actual list
                for item in items {
                    if let Ok(account) = parse_account_id(item) {
                        accounts.push(account);
                    }
                }
            }
        }
        ValueDef::Composite(Composite::Named(items)) => {
            // Check for "0" field (newtype) or iterate named fields
            for (name, item) in items {
                if name == "0" {
                    // This is a newtype wrapper, recurse into it
                    return parse_account_list(item);
                }
                if let Ok(account) = parse_account_id(item) {
                    accounts.push(account);
                }
            }
        }
        _ => {}
    }

    Ok(accounts)
}

fn parse_candidate_list<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<Vec<CandidateInfo>> {
    let mut candidates = Vec::new();

    match &value.value {
        // The value might be wrapped in a newtype (accessing .0)
        ValueDef::Composite(Composite::Unnamed(items)) => {
            // Check if this is a single-element wrapper or the actual list
            if items.len() == 1 {
                // Could be a newtype wrapper, try to recurse
                if let Ok(inner_candidates) = parse_candidate_list(&items[0]) {
                    if !inner_candidates.is_empty() {
                        return Ok(inner_candidates);
                    }
                }
                // Otherwise try to parse as a single candidate
                if let Ok(candidate) = parse_candidate_info(&items[0]) {
                    candidates.push(candidate);
                }
            } else {
                // Multiple items - this is the actual list
                for item in items {
                    if let Ok(candidate) = parse_candidate_info(item) {
                        candidates.push(candidate);
                    }
                }
            }
        }
        ValueDef::Composite(Composite::Named(items)) => {
            // Check for "0" field (newtype) or iterate named fields
            for (name, item) in items {
                if name == "0" {
                    // This is a newtype wrapper, recurse into it
                    return parse_candidate_list(item);
                }
                if let Ok(candidate) = parse_candidate_info(item) {
                    candidates.push(candidate);
                }
            }
        }
        _ => {}
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
    // Debug: print the value structure to understand the format
    // tracing::debug!("Parsing account from: {:?}", value);
    
    // Try to extract bytes from various representations
    match &value.value {
        // Direct 32-byte array as unnamed composite
        ValueDef::Composite(Composite::Unnamed(items)) => {
            if items.len() == 1 {
                // Wrapped in a single-element tuple, recurse
                return parse_account_id(&items[0]);
            } else if items.len() == 32 {
                // 32 individual bytes
                let mut bytes = [0u8; 32];
                for (i, item) in items.iter().enumerate() {
                    match &item.value {
                        ValueDef::Primitive(Primitive::U128(n)) => bytes[i] = *n as u8,
                        _ => return Err(anyhow::anyhow!("Expected u8 in account bytes")),
                    }
                }
                return Ok(AccountId32(bytes));
            }
        }
        // Named composite (might have an inner field)
        ValueDef::Composite(Composite::Named(fields)) => {
            // Look for common field names
            for (name, val) in fields {
                if name == "0" || name == "id" || name == "account" {
                    return parse_account_id(val);
                }
            }
        }
        // U256 primitive (32 bytes)
        ValueDef::Primitive(Primitive::U256(bytes)) => {
            let mut account_bytes = [0u8; 32];
            account_bytes.copy_from_slice(&bytes[..32]);
            return Ok(AccountId32(account_bytes));
        }
        _ => {}
    }

    Err(anyhow::anyhow!("Failed to parse AccountId32 from: {:?}", value))
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

fn parse_aura_authorities<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<Vec<AccountId32>> {
    // Aura authorities are stored as BoundedVec<Public, MaxAuthorities>
    // Public is a 32-byte sr25519 public key that maps to AccountId32
    let mut authorities = Vec::new();

    match &value.value {
        ValueDef::Composite(Composite::Unnamed(items)) => {
            // Could be newtype wrapper or actual list
            if items.len() == 1 {
                // Try to recurse into newtype
                if let Ok(inner) = parse_aura_authorities(&items[0]) {
                    if !inner.is_empty() {
                        return Ok(inner);
                    }
                }
            }
            // Parse as list of public keys
            for item in items {
                if let Ok(account) = parse_aura_public_key(item) {
                    authorities.push(account);
                }
            }
        }
        _ => {}
    }

    Ok(authorities)
}

fn parse_aura_public_key<T: std::fmt::Debug>(value: &ScaleValue<T>) -> Result<AccountId32> {
    // Aura Public key is a 32-byte array, same as AccountId32
    match &value.value {
        ValueDef::Composite(Composite::Unnamed(bytes)) => {
            if bytes.len() == 32 {
                let mut account_bytes = [0u8; 32];
                for (i, b) in bytes.iter().enumerate() {
                    if let ValueDef::Primitive(Primitive::U128(n)) = &b.value {
                        account_bytes[i] = *n as u8;
                    }
                }
                return Ok(AccountId32(account_bytes));
            }
            // Could be a wrapper
            if bytes.len() == 1 {
                return parse_aura_public_key(&bytes[0]);
            }
        }
        ValueDef::Composite(Composite::Named(fields)) => {
            // Look for inner field
            for (name, val) in fields {
                if name == "0" || name.to_lowercase().contains("inner") {
                    return parse_aura_public_key(val);
                }
            }
        }
        // Direct bytes representation
        _ => {
            // Try to extract as account
            if let Ok(account) = parse_account_id(value) {
                return Ok(account);
            }
        }
    }

    Err(anyhow::anyhow!("Failed to parse Aura public key: {:?}", value))
}
