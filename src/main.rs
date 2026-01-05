//! Collator Monitor - Automatic re-registration for Polkadot system chain collators
//!
//! This application monitors collator status across Polkadot and Kusama system chains
//! and automatically re-registers as a candidate if the collator falls out of the
//! candidate list or invulnerables list.

mod chain_client;
mod config;
mod error;
mod monitor;
mod slack;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::config::AppConfig;
use crate::monitor::{CollatorMonitor, MonitorStatus};

#[derive(Parser)]
#[command(name = "collator-monitor")]
#[command(about = "Monitor and auto-register collators on Polkadot system chains")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", global = true)]
    log_level: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Run monitoring once and exit
    Check,

    /// Run continuous monitoring on a schedule
    Watch {
        /// Check interval in seconds (overrides config)
        #[arg(long)]
        interval: Option<u64>,
    },

    /// Show current collator status on all chains (no registration)
    Status,

    /// Show configuration (for debugging)
    ShowConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&cli.log_level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    info!("Collator Monitor starting up");

    // Load configuration
    let config = AppConfig::load()?;

    match cli.command {
        Commands::Check => {
            run_check(config).await?;
        }
        Commands::Watch { interval } => {
            let interval_secs = interval.unwrap_or(config.check_interval_secs);
            run_watch(config, interval_secs).await?;
        }
        Commands::Status => {
            run_status(config).await?;
        }
        Commands::ShowConfig => {
            println!("{:#?}", config);
        }
    }

    Ok(())
}

async fn run_check(config: AppConfig) -> Result<()> {
    info!("Running single check across all chains");

    let monitor = CollatorMonitor::new(config)?;
    let results = monitor.monitor_all_chains().await;

    print_results(&results);

    // Check if any errors occurred
    let has_errors = results
        .iter()
        .any(|r| matches!(r.status, MonitorStatus::Error(_)));

    if has_errors {
        error!("Some chains had errors during monitoring");
        std::process::exit(1);
    }

    Ok(())
}

async fn run_watch(config: AppConfig, interval_secs: u64) -> Result<()> {
    info!(
        "Starting continuous monitoring with {} second interval",
        interval_secs
    );

    let monitor = CollatorMonitor::new(config)?;

    loop {
        info!("Running scheduled check");
        let results = monitor.monitor_all_chains().await;
        print_results(&results);

        info!("Next check in {} seconds", interval_secs);
        tokio::time::sleep(tokio::time::Duration::from_secs(interval_secs)).await;
    }
}

async fn run_status(config: AppConfig) -> Result<()> {
    info!("Checking collator status across all chains (read-only)");

    use crate::chain_client::ChainClient;
    use crate::config::{chain_supports_proxy, default_rpc_url, Network, SystemChain};

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

    println!("\n=== Polkadot System Chains ===");
    println!("Collator address: {}\n", config.polkadot_collator_address);

    for chain in polkadot_chains {
        if !chain_supports_proxy(chain) {
            println!(
                "  {}: Skipped (no proxy support)",
                chain.display_name(Network::Polkadot)
            );
            continue;
        }

        let rpc_url = config
            .chain_config(Network::Polkadot, chain)
            .map(|c| c.rpc_url.as_str())
            .unwrap_or_else(|| default_rpc_url(Network::Polkadot, chain));

        match ChainClient::connect(rpc_url, Network::Polkadot, chain).await {
            Ok(client) => {
                let account = client.parse_address(&config.polkadot_collator_address)?;
                let status = client.get_collator_status(&account).await?;
                let balance = client.get_free_balance(&account).await?;
                let bond = client.get_candidacy_bond().await?;

                println!(
                    "  {}: {:?}, Balance: {:.4} DOT, Min Bond: {:.4} DOT",
                    chain.display_name(Network::Polkadot),
                    status,
                    balance as f64 / 10_000_000_000.0,
                    bond as f64 / 10_000_000_000.0
                );
            }
            Err(e) => {
                println!(
                    "  {}: Error - {}",
                    chain.display_name(Network::Polkadot),
                    e
                );
            }
        }
    }

    println!("\n=== Kusama System Chains ===");
    println!("Collator address: {}\n", config.kusama_collator_address);

    for chain in kusama_chains {
        if !chain_supports_proxy(chain) {
            println!(
                "  {}: Skipped (no proxy support)",
                chain.display_name(Network::Kusama)
            );
            continue;
        }

        let rpc_url = config
            .chain_config(Network::Kusama, chain)
            .map(|c| c.rpc_url.as_str())
            .unwrap_or_else(|| default_rpc_url(Network::Kusama, chain));

        match ChainClient::connect(rpc_url, Network::Kusama, chain).await {
            Ok(client) => {
                let account = client.parse_address(&config.kusama_collator_address)?;
                let status = client.get_collator_status(&account).await?;
                let balance = client.get_free_balance(&account).await?;
                let bond = client.get_candidacy_bond().await?;

                println!(
                    "  {}: {:?}, Balance: {:.4} KSM, Min Bond: {:.4} KSM",
                    chain.display_name(Network::Kusama),
                    status,
                    balance as f64 / 1_000_000_000_000.0,
                    bond as f64 / 1_000_000_000_000.0
                );
            }
            Err(e) => {
                println!("  {}: Error - {}", chain.display_name(Network::Kusama), e);
            }
        }
    }

    Ok(())
}

fn print_results(results: &[crate::monitor::MonitorResult]) {
    println!("\n=== Monitoring Results ===\n");

    for result in results {
        let status_str = match &result.status {
            MonitorStatus::AlreadyCollator(s) => format!("✓ Already collator: {:?}", s),
            MonitorStatus::RegisteredAsCandidate { bond, tx_hash } => {
                format!("✓ Registered with bond {} (tx: {})", bond, tx_hash)
            }
            MonitorStatus::UpdatedBond { old_bond, new_bond, tx_hash } => {
                format!("✓ Updated bond {} → {} (tx: {})", old_bond, new_bond, tx_hash)
            }
            MonitorStatus::InsufficientFunds { available, required } => {
                format!("✗ Insufficient funds: have {}, need {}", available, required)
            }
            MonitorStatus::Error(e) => format!("✗ Error: {}", e),
            MonitorStatus::Skipped(reason) => format!("- Skipped: {}", reason),
        };

        println!("  {}: {}", result.chain_name, status_str);
    }

    println!();
}
