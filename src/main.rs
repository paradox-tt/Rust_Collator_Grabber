//! Collator Monitor - Automatic re-registration for Polkadot system chain collators
//!
//! This application monitors collator status across Polkadot and Kusama system chains
//! and automatically re-registers as a candidate if the collator falls out of the
//! candidate list or invulnerables list.

mod block_tracker;
mod chain_client;
mod config;
mod error;
mod metadata;
mod monitor;
mod slack;

use std::sync::Arc;
use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::block_tracker::BlockTracker;
use crate::config::AppConfig;
use crate::monitor::{CollatorMonitor, MonitorStatus};
use crate::slack::SlackNotifier;

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

    // For single check, we don't use background block tracker
    let block_tracker = Arc::new(BlockTracker::new());
    let monitor = CollatorMonitor::new(config, block_tracker)?;
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
    let summary_interval_secs = config.summary_interval_secs;
    
    info!(
        "Starting continuous monitoring with {} second interval, summary every {} seconds",
        interval_secs, summary_interval_secs
    );

    // Create slack notifier - prefer bot token for full functionality
    let slack = Arc::new(
        if let (Some(bot_token), Some(channel)) = (&config.slack_bot_token, &config.slack_channel) {
            info!("Using Slack bot token (message update/delete enabled)");
            SlackNotifier::with_bot_token(
                bot_token.clone(),
                channel.clone(),
                config.slack_user_ids_onchain.clone(),
                config.slack_user_ids_ops.clone(),
            )
        } else {
            info!("Using Slack webhook (message update/delete disabled)");
            SlackNotifier::new(
                config.slack_webhook_url.clone(),
                config.slack_user_ids_onchain.clone(),
                config.slack_user_ids_ops.clone(),
            )
        }
    );

    // Start background block trackers with slack integration
    let block_tracker = Arc::new(BlockTracker::new());
    let _tracker_handles = block_tracker.clone().start_tracking(config.clone(), slack.clone());
    
    // Give trackers a moment to initialize
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let monitor = CollatorMonitor::new(config, block_tracker.clone())?;
    
    let mut last_summary = std::time::Instant::now();
    // Send initial summary
    info!("Sending initial status summary");
    let slots = monitor.collect_slot_info().await;
    let _ = monitor.slack().send_status_summary(&slots).await;

    loop {
        info!("Running scheduled check");
        let results = monitor.monitor_all_chains().await;
        print_results(&results);

        // Check if it's time to send a summary
        if last_summary.elapsed().as_secs() >= summary_interval_secs {
            info!("Sending periodic status summary");
            let slots = monitor.collect_slot_info().await;
            let _ = monitor.slack().send_status_summary(&slots).await;
            last_summary = std::time::Instant::now();
        }

        info!("Next check in {} seconds", interval_secs);
        tokio::time::sleep(tokio::time::Duration::from_secs(interval_secs)).await;
    }
    
    // Cleanup (unreachable in normal operation, but good practice)
    #[allow(unreachable_code)]
    {
        block_tracker.shutdown().await;
        Ok(())
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
    println!("Looking for collator: {}\n", config.polkadot_collator_address);

    for chain in polkadot_chains {
        let supports_proxy = chain_supports_proxy(chain);
        let read_only_marker = if !supports_proxy { " [READ-ONLY - no proxy support]" } else { "" };
        
        let rpc_urls = config.get_rpc_urls(Network::Polkadot, chain);
        let rpc_url = rpc_urls.first().map(|s| s.as_str()).unwrap_or_else(|| default_rpc_url(Network::Polkadot, chain));

        match ChainClient::connect(rpc_url, Network::Polkadot, chain).await {
            Ok(client) => {
                let account = client.parse_address(&config.polkadot_collator_address)?;
                let status = client.get_collator_status(&account).await?;
                let balance = client.get_free_balance(&account).await?;
                let min_bond = client.get_candidacy_bond().await?;
                
                // Get invulnerables and candidates for display
                let invulnerables = client.get_invulnerables().await?;
                let candidates = client.get_candidates().await?;
                
                // Calculate competitive bond info
                let lowest_candidate_bond = candidates.iter().filter(|c| c.deposit > 0).map(|c| c.deposit).min();
                let highest_candidate_bond = candidates.iter().map(|c| c.deposit).max();
                
                let decimals = 10_000_000_000.0; // DOT decimals

                println!("  {}{}:", chain.display_name(Network::Polkadot), read_only_marker);
                println!("    Your Status: {:?}", status);
                println!(
                    "    Your Balance: {:.4} DOT",
                    balance as f64 / decimals
                );
                println!("    Bond Requirements:");
                println!("      - Minimum to register: {:.4} DOT", min_bond as f64 / decimals);
                if let Some(lowest) = lowest_candidate_bond {
                    println!("      - To beat lowest candidate: {:.4} DOT", (lowest + 1) as f64 / decimals);
                }
                if let Some(highest) = highest_candidate_bond {
                    println!("      - To be top candidate: {:.4} DOT", (highest + 1) as f64 / decimals);
                }
                
                // Show if user can compete
                let reserve = 10_000_000_000u128; // 1 DOT reserve
                let available = balance.saturating_sub(reserve);
                println!("    Your Available for Bond: {:.4} DOT (after 1 DOT reserve)", available as f64 / decimals);
                
                if let Some(lowest) = lowest_candidate_bond {
                    if available > lowest {
                        println!("    ✓ Can beat lowest candidate");
                    } else {
                        let needed = lowest.saturating_sub(available) + 1;
                        println!("    ✗ Need {:.4} more DOT to beat lowest candidate", needed as f64 / decimals);
                    }
                }
                if let Some(highest) = highest_candidate_bond {
                    if available > highest {
                        println!("    ✓ Can be top candidate");
                    } else {
                        let needed = highest.saturating_sub(available) + 1;
                        println!("    ✗ Need {:.4} more DOT to be top candidate", needed as f64 / decimals);
                    }
                }
                
                println!("    Invulnerables ({}):", invulnerables.len());
                for inv in &invulnerables {
                    let marker = if inv == &account { " <-- YOU" } else { "" };
                    println!("      - {}{}", inv, marker);
                }
                println!("    Candidates ({}):", candidates.len());
                for cand in &candidates {
                    let marker = if cand.who == account { " <-- YOU" } else { "" };
                    println!(
                        "      - {} (bond: {:.4} DOT){}",
                        cand.who,
                        cand.deposit as f64 / decimals,
                        marker
                    );
                }
                println!();
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
    println!("Looking for collator: {}\n", config.kusama_collator_address);

    for chain in kusama_chains {
        let supports_proxy = chain_supports_proxy(chain);
        let read_only_marker = if !supports_proxy { " [READ-ONLY - no proxy support]" } else { "" };

        let rpc_urls = config.get_rpc_urls(Network::Kusama, chain);
        let rpc_url = rpc_urls.first().map(|s| s.as_str()).unwrap_or_else(|| default_rpc_url(Network::Kusama, chain));

        match ChainClient::connect(rpc_url, Network::Kusama, chain).await {
            Ok(client) => {
                let account = client.parse_address(&config.kusama_collator_address)?;
                let status = client.get_collator_status(&account).await?;
                let balance = client.get_free_balance(&account).await?;
                let min_bond = client.get_candidacy_bond().await?;
                
                // Get invulnerables and candidates for display
                let invulnerables = client.get_invulnerables().await?;
                let candidates = client.get_candidates().await?;
                
                // Calculate competitive bond info
                let lowest_candidate_bond = candidates.iter().filter(|c| c.deposit > 0).map(|c| c.deposit).min();
                let highest_candidate_bond = candidates.iter().map(|c| c.deposit).max();
                
                let decimals = 1_000_000_000_000.0; // KSM decimals

                println!("  {}{}:", chain.display_name(Network::Kusama), read_only_marker);
                println!("    Your Status: {:?}", status);
                println!(
                    "    Your Balance: {:.4} KSM",
                    balance as f64 / decimals
                );
                println!("    Bond Requirements:");
                println!("      - Minimum to register: {:.4} KSM", min_bond as f64 / decimals);
                if let Some(lowest) = lowest_candidate_bond {
                    println!("      - To beat lowest candidate: {:.4} KSM", (lowest + 1) as f64 / decimals);
                }
                if let Some(highest) = highest_candidate_bond {
                    println!("      - To be top candidate: {:.4} KSM", (highest + 1) as f64 / decimals);
                }
                
                // Show if user can compete
                let reserve = 100_000_000_000u128; // 0.1 KSM reserve
                let available = balance.saturating_sub(reserve);
                println!("    Your Available for Bond: {:.4} KSM (after 0.1 KSM reserve)", available as f64 / decimals);
                
                if let Some(lowest) = lowest_candidate_bond {
                    if available > lowest {
                        println!("    ✓ Can beat lowest candidate");
                    } else {
                        let needed = lowest.saturating_sub(available) + 1;
                        println!("    ✗ Need {:.4} more KSM to beat lowest candidate", needed as f64 / decimals);
                    }
                }
                if let Some(highest) = highest_candidate_bond {
                    if available > highest {
                        println!("    ✓ Can be top candidate");
                    } else {
                        let needed = highest.saturating_sub(available) + 1;
                        println!("    ✗ Need {:.4} more KSM to be top candidate", needed as f64 / decimals);
                    }
                }
                
                println!("    Invulnerables ({}):", invulnerables.len());
                for inv in &invulnerables {
                    let marker = if inv == &account { " <-- YOU" } else { "" };
                    println!("      - {}{}", inv, marker);
                }
                println!("    Candidates ({}):", candidates.len());
                for cand in &candidates {
                    let marker = if cand.who == account { " <-- YOU" } else { "" };
                    println!(
                        "      - {} (bond: {:.4} KSM){}",
                        cand.who,
                        cand.deposit as f64 / decimals,
                        marker
                    );
                }
                println!();
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
            MonitorStatus::CannotCompete { available, lowest_candidate, needed } => {
                format!("✗ Cannot compete: have {}, lowest candidate {}, need {} more", 
                    available, lowest_candidate, needed)
            }
            MonitorStatus::ManualActionRequired { reason, current_status } => {
                format!("🔧 Manual action required: {} (current: {:?})", reason, current_status)
            }
            MonitorStatus::Error(e) => format!("✗ Error: {}", e),
            MonitorStatus::Skipped(reason) => format!("- Skipped: {}", reason),
        };

        println!("  {}: {}", result.chain_name, status_str);
    }

    println!();
}
