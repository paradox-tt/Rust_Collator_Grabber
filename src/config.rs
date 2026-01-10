//! Configuration types for the collator monitor.

use serde::Deserialize;
use std::collections::HashMap;

/// Network type (Polkadot or Kusama ecosystem)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Polkadot,
    Kusama,
}

impl Network {
    /// Get the token decimals for this network
    pub fn decimals(&self) -> u32 {
        match self {
            Network::Polkadot => 10, // DOT has 10 decimals
            Network::Kusama => 12,   // KSM has 12 decimals
        }
    }

    /// Get the reserve amount to keep (1 DOT or 0.1 KSM)
    pub fn reserve_amount(&self) -> u128 {
        match self {
            Network::Polkadot => 1 * 10u128.pow(10),  // 1 DOT
            Network::Kusama => 10u128.pow(11),        // 0.1 KSM
        }
    }

    /// Get the token symbol
    pub fn symbol(&self) -> &'static str {
        match self {
            Network::Polkadot => "DOT",
            Network::Kusama => "KSM",
        }
    }
}

/// Chain identifier for system chains
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemChain {
    AssetHub,
    BridgeHub,
    Collectives,
    Coretime,
    People,
    Encointer,
}

impl SystemChain {
    /// Get the network this chain belongs to
    pub fn valid_networks(&self) -> Vec<Network> {
        match self {
            SystemChain::AssetHub => vec![Network::Polkadot, Network::Kusama],
            SystemChain::BridgeHub => vec![Network::Polkadot, Network::Kusama],
            SystemChain::Collectives => vec![Network::Polkadot], // Only on Polkadot
            SystemChain::Coretime => vec![Network::Polkadot, Network::Kusama],
            SystemChain::People => vec![Network::Polkadot, Network::Kusama],
            SystemChain::Encointer => vec![Network::Kusama], // Only on Kusama
        }
    }

    /// Get display name
    pub fn display_name(&self, network: Network) -> String {
        let chain_name = match self {
            SystemChain::AssetHub => "Asset Hub",
            SystemChain::BridgeHub => "Bridge Hub",
            SystemChain::Collectives => "Collectives",
            SystemChain::Coretime => "Coretime",
            SystemChain::People => "People",
            SystemChain::Encointer => "Encointer",
        };
        format!("{} {}", network.symbol(), chain_name)
    }
}

/// Configuration for a single chain endpoint
#[derive(Debug, Clone, Deserialize)]
pub struct ChainConfig {
    /// RPC WebSocket URL
    pub rpc_url: String,
    /// Whether to monitor this chain
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// Main application configuration
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// Collator account address for Polkadot chains (SS58 format)
    pub polkadot_collator_address: String,

    /// Collator account address for Kusama chains (SS58 format)
    pub kusama_collator_address: String,

    /// Proxy account seed (hex or mnemonic)
    /// This is the account that will sign transactions on behalf of the collator
    /// The proxy should be configured as NonTransfer type
    pub proxy_seed: String,

    /// Slack webhook URL for notifications
    pub slack_webhook_url: Option<String>,

    /// Check interval in seconds (for continuous monitoring mode)
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,

    /// Chain-specific configurations
    /// Key format: "network_chain" e.g., "polkadot_assethub"
    #[serde(default)]
    pub chains: HashMap<String, ChainConfig>,
}

fn default_check_interval() -> u64 {
    3600 // 1 hour
}

impl AppConfig {
    /// Get the collator address for a given network
    pub fn collator_address(&self, network: Network) -> &str {
        match network {
            Network::Polkadot => &self.polkadot_collator_address,
            Network::Kusama => &self.kusama_collator_address,
        }
    }

    /// Get chain config for a specific network and chain
    pub fn chain_config(&self, network: Network, chain: SystemChain) -> Option<&ChainConfig> {
        let key = format!(
            "{}_{}",
            match network {
                Network::Polkadot => "polkadot",
                Network::Kusama => "kusama",
            },
            match chain {
                SystemChain::AssetHub => "assethub",
                SystemChain::BridgeHub => "bridgehub",
                SystemChain::Collectives => "collectives",
                SystemChain::Coretime => "coretime",
                SystemChain::People => "people",
                SystemChain::Encointer => "encointer",
            }
        );
        self.chains.get(&key)
    }

    /// Load configuration from environment and config file
    pub fn load() -> anyhow::Result<Self> {
        // Load .env file if present - try multiple locations
        // 1. Explicit path from ENV_FILE environment variable
        // 2. Config subdirectory (config/.env) - for service deployment
        // 3. Current directory (.env) - fallback
        if let Ok(env_path) = std::env::var("ENV_FILE") {
            let _ = dotenvy::from_path(&env_path);
        } else {
            // Try config/.env first (for service deployment)
            let config_env = std::path::Path::new("config/.env");
            if config_env.exists() {
                let _ = dotenvy::from_path(config_env);
            } else {
                // Fall back to .env in current directory
                let _ = dotenvy::dotenv();
            }
        }

        // Build config from environment variables
        // We use __ (double underscore) as separator for nested keys
        // This means:
        //   COLLATOR_POLKADOT_COLLATOR_ADDRESS -> polkadot_collator_address (flat)
        //   COLLATOR_CHAINS__POLKADOT_ASSETHUB__RPC_URL -> chains.polkadot_assethub.rpc_url (nested)
        let config = config::Config::builder()
            // Load from config.toml if present (try both locations)
            .add_source(config::File::with_name("config/config").required(false))
            .add_source(config::File::with_name("config").required(false))
            // Override with environment variables (prefixed with COLLATOR_)
            // Using __ as separator so single _ stays as part of the key name
            .add_source(
                config::Environment::with_prefix("COLLATOR")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        config.try_deserialize().map_err(Into::into)
    }
}

/// Default RPC endpoints for system chains (LuckyFriday endpoints)
pub fn default_rpc_url(network: Network, chain: SystemChain) -> &'static str {
    match (network, chain) {
        // Polkadot system chains (LuckyFriday)
        (Network::Polkadot, SystemChain::AssetHub) => "wss://rpc-asset-hub-polkadot.luckyfriday.io",
        (Network::Polkadot, SystemChain::BridgeHub) => "wss://rpc-bridge-hub-polkadot.luckyfriday.io",
        (Network::Polkadot, SystemChain::Collectives) => "wss://rpc-collectives-polkadot.luckyfriday.io",
        (Network::Polkadot, SystemChain::Coretime) => "wss://rpc-coretime-polkadot.luckyfriday.io",
        (Network::Polkadot, SystemChain::People) => "wss://rpc-people-polkadot.luckyfriday.io",

        // Kusama system chains (LuckyFriday)
        (Network::Kusama, SystemChain::AssetHub) => "wss://rpc-asset-hub-kusama.luckyfriday.io",
        (Network::Kusama, SystemChain::BridgeHub) => "wss://rpc-bridge-hub-kusama.luckyfriday.io",
        (Network::Kusama, SystemChain::Coretime) => "wss://rpc-coretime-kusama.luckyfriday.io",
        (Network::Kusama, SystemChain::People) => "wss://rpc-people-kusama.luckyfriday.io",
        (Network::Kusama, SystemChain::Encointer) => "wss://rpc-encointer-kusama.luckyfriday.io",

        // Invalid combinations
        (Network::Polkadot, SystemChain::Encointer) => panic!("Encointer is only on Kusama"),
        (Network::Kusama, SystemChain::Collectives) => panic!("Collectives is only on Polkadot"),
    }
}

/// Check if a chain supports proxy accounts for collator registration
/// BridgeHub doesn't support proxy accounts, so it's read-only (status check only)
pub fn chain_supports_proxy(chain: SystemChain) -> bool {
    match chain {
        // BridgeHub doesn't support proxy accounts for collator registration
        SystemChain::BridgeHub => false,
        // All other chains support proxy
        _ => true,
    }
}
