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
    /// Primary RPC WebSocket URL (for backwards compatibility)
    #[serde(default)]
    pub rpc_url: Option<String>,
    /// List of RPC WebSocket URLs (primary + fallbacks)
    #[serde(default)]
    pub rpc_urls: Vec<String>,
    /// Whether to monitor this chain
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl ChainConfig {
    /// Get all RPC URLs in order of preference
    pub fn get_rpc_urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        
        // Add single rpc_url first if present
        if let Some(url) = &self.rpc_url {
            urls.push(url.clone());
        }
        
        // Add all rpc_urls
        for url in &self.rpc_urls {
            if !urls.contains(url) {
                urls.push(url.clone());
            }
        }
        
        urls
    }
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

    /// Slack webhook URL for notifications (simpler, but can't update/delete)
    pub slack_webhook_url: Option<String>,

    /// Slack bot token for full API access (enables update/delete messages)
    /// Get from https://api.slack.com/apps -> OAuth & Permissions
    pub slack_bot_token: Option<String>,

    /// Slack channel ID or name (required when using bot token)
    pub slack_channel: Option<String>,

    /// Slack user IDs to ping for ON-CHAIN actions (registration, bond updates, manual actions)
    /// These are people who can submit transactions
    /// Format: U08CUCTA3R7,U12345ABCD
    pub slack_user_ids_onchain: Vec<String>,

    /// Slack user IDs to ping for OPS issues (block production, disconnections)
    /// These are people who can check/fix infrastructure issues
    /// Format: U08CUCTA3R7,U12345ABCD
    pub slack_user_ids_ops: Vec<String>,

    /// Check interval in seconds (for continuous monitoring mode)
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,

    /// Summary interval in seconds (for periodic status summary)
    #[serde(default = "default_summary_interval")]
    pub summary_interval_secs: u64,

    /// Chain-specific configurations
    /// Key format: "network_chain" e.g., "polkadot_assethub"
    #[serde(default)]
    pub chains: HashMap<String, ChainConfig>,
}

fn default_check_interval() -> u64 {
    3600 // 1 hour
}

fn default_summary_interval() -> u64 {
    21600 // 6 hours
}

impl AppConfig {
    /// Get the collator address for a given network
    pub fn collator_address(&self, network: Network) -> &str {
        match network {
            Network::Polkadot => &self.polkadot_collator_address,
            Network::Kusama => &self.kusama_collator_address,
        }
    }

    /// Get all RPC URLs for a chain (from config or defaults)
    pub fn get_rpc_urls(&self, network: Network, chain: SystemChain) -> Vec<String> {
        // Check if there's a custom config
        if let Some(config) = self.chain_config(network, chain) {
            let urls = config.get_rpc_urls();
            if !urls.is_empty() {
                return urls;
            }
        }
        
        // Fall back to defaults
        default_rpc_urls(network, chain)
            .into_iter()
            .map(|s| s.to_string())
            .collect()
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
            let config_env = std::path::Path::new("config/.env");
            if config_env.exists() {
                let _ = dotenvy::from_path(config_env);
            } else {
                let _ = dotenvy::dotenv();
            }
        }

        // Read required environment variables
        let polkadot_address = std::env::var("COLLATOR_POLKADOT_COLLATOR_ADDRESS")
            .map_err(|_| anyhow::anyhow!("COLLATOR_POLKADOT_COLLATOR_ADDRESS not set"))?;
        let kusama_address = std::env::var("COLLATOR_KUSAMA_COLLATOR_ADDRESS")
            .map_err(|_| anyhow::anyhow!("COLLATOR_KUSAMA_COLLATOR_ADDRESS not set"))?;
        let proxy_seed = std::env::var("COLLATOR_PROXY_SEED")
            .map_err(|_| anyhow::anyhow!("COLLATOR_PROXY_SEED not set"))?;
        
        // Read optional environment variables
        let slack_webhook = std::env::var("COLLATOR_SLACK_WEBHOOK_URL").ok();
        
        // On-chain action user IDs (for registration, bond updates, manual actions)
        let slack_user_ids_onchain = std::env::var("COLLATOR_SLACK_USER_IDS_ONCHAIN")
            .ok()
            .map(|s| s.split(',').map(|id| id.trim().to_string()).filter(|id| !id.is_empty()).collect())
            .unwrap_or_default();
        
        // Ops user IDs (for block production issues, disconnections)
        let slack_user_ids_ops = std::env::var("COLLATOR_SLACK_USER_IDS_OPS")
            .ok()
            .map(|s| s.split(',').map(|id| id.trim().to_string()).filter(|id| !id.is_empty()).collect())
            .unwrap_or_default();
        
        let check_interval = std::env::var("COLLATOR_CHECK_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600u64);
        let summary_interval = std::env::var("COLLATOR_SUMMARY_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(21600u64); // 6 hours

        // Load Slack bot token and channel (for advanced features)
        let slack_bot_token = std::env::var("COLLATOR_SLACK_BOT_TOKEN").ok();
        let slack_channel = std::env::var("COLLATOR_SLACK_CHANNEL").ok();

        // Load chain configs from config.toml if present
        let chains = Self::load_chain_configs()?;

        Ok(Self {
            polkadot_collator_address: polkadot_address,
            kusama_collator_address: kusama_address,
            proxy_seed,
            slack_webhook_url: slack_webhook,
            slack_bot_token,
            slack_channel,
            slack_user_ids_onchain,
            slack_user_ids_ops,
            check_interval_secs: check_interval,
            summary_interval_secs: summary_interval,
            chains,
        })
    }

    /// Load chain-specific configs from config.toml
    fn load_chain_configs() -> anyhow::Result<HashMap<String, ChainConfig>> {
        // Try to load from config files
        let config_result = config::Config::builder()
            .add_source(config::File::with_name("config/config").required(false))
            .add_source(config::File::with_name("config").required(false))
            .build();

        if let Ok(config) = config_result {
            if let Ok(chains) = config.get::<HashMap<String, ChainConfig>>("chains") {
                return Ok(chains);
            }
        }

        Ok(HashMap::new())
    }
}

/// Default RPC endpoints for system chains
/// Returns array of URLs: [LuckyFriday (primary), Stakeworld (fallback), Dotters (fallback)]
pub fn default_rpc_urls(network: Network, chain: SystemChain) -> Vec<&'static str> {
    match (network, chain) {
        // Polkadot system chains
        (Network::Polkadot, SystemChain::AssetHub) => vec![
            "wss://rpc-asset-hub-polkadot.luckyfriday.io",
            "wss://dot-asset-hub-rpc.stakeworld.io",
            "wss://statemint-rpc.dotters.network",
        ],
        (Network::Polkadot, SystemChain::BridgeHub) => vec![
            "wss://rpc-bridge-hub-polkadot.luckyfriday.io",
            "wss://dot-bridge-hub-rpc.stakeworld.io",
            "wss://bridge-hub-polkadot-rpc.dotters.network",
        ],
        (Network::Polkadot, SystemChain::Collectives) => vec![
            "wss://rpc-collectives-polkadot.luckyfriday.io",
            "wss://dot-collectives-rpc.stakeworld.io",
            "wss://collectives-polkadot-rpc.dotters.network",
        ],
        (Network::Polkadot, SystemChain::Coretime) => vec![
            "wss://rpc-coretime-polkadot.luckyfriday.io",
            "wss://dot-coretime-rpc.stakeworld.io",
            "wss://coretime-polkadot-rpc.dotters.network",
        ],
        (Network::Polkadot, SystemChain::People) => vec![
            "wss://rpc-people-polkadot.luckyfriday.io",
            "wss://dot-people-rpc.stakeworld.io",
            "wss://people-polkadot-rpc.dotters.network",
        ],

        // Kusama system chains
        (Network::Kusama, SystemChain::AssetHub) => vec![
            "wss://rpc-asset-hub-kusama.luckyfriday.io",
            "wss://ksm-asset-hub-rpc.stakeworld.io",
            "wss://statemine-rpc.dotters.network",
        ],
        (Network::Kusama, SystemChain::BridgeHub) => vec![
            "wss://rpc-bridge-hub-kusama.luckyfriday.io",
            "wss://ksm-bridge-hub-rpc.stakeworld.io",
            "wss://bridge-hub-kusama-rpc.dotters.network",
        ],
        (Network::Kusama, SystemChain::Coretime) => vec![
            "wss://rpc-coretime-kusama.luckyfriday.io",
            "wss://ksm-coretime-rpc.stakeworld.io",
            "wss://coretime-kusama-rpc.dotters.network",
        ],
        (Network::Kusama, SystemChain::People) => vec![
            "wss://rpc-people-kusama.luckyfriday.io",
            "wss://ksm-people-rpc.stakeworld.io",
            "wss://people-kusama-rpc.dotters.network",
        ],
        (Network::Kusama, SystemChain::Encointer) => vec![
            "wss://rpc-encointer-kusama.luckyfriday.io",
            "wss://ksm-encointer-rpc.stakeworld.io",
            "wss://encointer-kusama-rpc.dotters.network",
        ],

        // Invalid combinations
        (Network::Polkadot, SystemChain::Encointer) => panic!("Encointer is only on Kusama"),
        (Network::Kusama, SystemChain::Collectives) => panic!("Collectives is only on Polkadot"),
    }
}

/// Legacy function for backwards compatibility - returns first URL
pub fn default_rpc_url(network: Network, chain: SystemChain) -> &'static str {
    default_rpc_urls(network, chain)[0]
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
