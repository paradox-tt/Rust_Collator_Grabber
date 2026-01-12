# Collator Monitor

Automatic monitoring and re-registration tool for Polkadot/Kusama system chain collators.

## Overview

This tool monitors your collator status across all Polkadot and Kusama system chains and automatically re-registers you as a candidate if you fall out of the collator set. It supports:

- **Polkadot chains**: Asset Hub, Bridge Hub, Collectives, Coretime, People
- **Kusama chains**: Asset Hub, Bridge Hub, Coretime, People, Encointer

## Features

- ✅ Monitors both invulnerable and candidate collator status
- ✅ Automatic registration via proxy account
- ✅ Automatic bond increase to maximize competitiveness
- ✅ Slack notifications for alerts and successes
- ✅ Configurable check intervals
- ✅ Support for custom RPC endpoints
- ✅ Dry-run status check mode

## Prerequisites

- **Rust 1.83+** (required for subxt 0.44.0)
- A collator account with:
  - Registered session keys on each chain
  - Sufficient balance for the candidacy bond
- A proxy account configured to act on behalf of your collator
  - The proxy type should be `NonTransfer`
  - The proxy account needs funds for transaction fees

## Installation

```bash
# Clone the repository
git clone <your-repo-url>
cd collator-monitor

# Build in release mode
cargo build --release

# The binary will be at ./target/release/collator-monitor
```

## Configuration

### Option 1: Configuration File

Copy `config.example.toml` to `config.toml` and edit:

```toml
polkadot_collator_address = "1YourPolkadotAddress..."
kusama_collator_address = "CYourKusamaAddress..."
proxy_seed = "your mnemonic phrase here"
slack_webhook_url = "https://hooks.slack.com/services/..."
```

### Option 2: Environment Variables

Copy `.env.example` to `.env` and edit:

```bash
COLLATOR_POLKADOT_COLLATOR_ADDRESS=1YourPolkadotAddress...
COLLATOR_KUSAMA_COLLATOR_ADDRESS=CYourKusamaAddress...
COLLATOR_PROXY_SEED="your mnemonic phrase here"
COLLATOR_SLACK_WEBHOOK_URL=https://hooks.slack.com/services/...
```

### Proxy Account Seed Formats

The proxy seed can be provided in several formats:

1. **Mnemonic phrase**: `"word1 word2 word3 ... word12"` (or 24 words)
2. **Hex seed**: `"0x1234567890abcdef..."` (64 hex characters = 32 bytes)
3. **URI with derivation**: `"//Alice"` (for development/testing)

## Usage

### Check Status (Read-Only)

View the current collator status across all chains without making any changes:

```bash
./collator-monitor status
```

### Run Once

Check all chains and register if needed, then exit:

```bash
./collator-monitor check
```

### Continuous Monitoring

Run continuously, checking at regular intervals:

```bash
# Use interval from config (default: 1 hour)
./collator-monitor watch

# Override interval (e.g., check every 30 minutes)
./collator-monitor watch --interval 1800
```

### Debug Configuration

Show the loaded configuration:

```bash
./collator-monitor show-config
```

### Logging

Control log verbosity with the `--log-level` flag or `RUST_LOG` environment variable:

```bash
# Via flag
./collator-monitor --log-level debug check

# Via environment
RUST_LOG=debug ./collator-monitor check
```

## How It Works

### Monitoring Logic

For each system chain, the tool:

1. **Checks invulnerables list**: If your account is an invulnerable, no action needed
2. **Checks candidate list**: If you're a candidate, checks if bond can be increased
3. **Attempts registration** if not found:
   - Gets the candidacy bond requirement
   - Checks your account balance
   - Registers with maximum possible bond (balance - reserve)
   - Alerts via Slack if insufficient funds

### Bond Management

The tool always tries to maximize your bond to stay competitive:

- **Polkadot**: Uses `available_balance - 1 DOT` as the bond
- **Kusama**: Uses `available_balance - 0.1 KSM` as the bond

If you're already registered but have more funds available, the tool will automatically increase your bond.

### Proxy Transactions

All transactions are submitted through the proxy pallet using **NonTransfer** proxy type:

```
proxy.proxy(collator_account, Some(NonTransfer), inner_call)
```

Where `inner_call` is one of:
- `collatorSelection.registerAsCandidate()`
- `collatorSelection.updateBond(new_bond)`

### RPC Endpoints

By default, the tool uses LuckyFriday RPC endpoints for all system chains:

**Polkadot:**
- Asset Hub: `wss://rpc-asset-hub-polkadot.luckyfriday.io`
- ~~Bridge Hub~~: Does not support proxy accounts
- Collectives: `wss://rpc-collectives-polkadot.luckyfriday.io`
- Coretime: `wss://rpc-coretime-polkadot.luckyfriday.io`
- People: `wss://rpc-people-polkadot.luckyfriday.io`

**Kusama:**
- Asset Hub: `wss://rpc-asset-hub-kusama.luckyfriday.io`
- ~~Bridge Hub~~: Does not support proxy accounts
- Coretime: `wss://rpc-coretime-kusama.luckyfriday.io`
- People: `wss://rpc-people-kusama.luckyfriday.io`
- Encointer: `wss://rpc-encointer-kusama.luckyfriday.io`

> **Note:** BridgeHub chains on both Polkadot and Kusama do not support proxy accounts for collator registration and are automatically skipped.

## Slack Notifications

When configured, the tool sends Slack notifications for:

- ⚠️ **Insufficient Funds**: Cannot register due to low balance
- ✅ **Registration Success**: Successfully registered as a candidate
- ❌ **Errors**: Any errors during monitoring

## Running as a Service

### Systemd (Linux)

Create `/etc/systemd/system/collator-monitor.service`:

```ini
[Unit]
Description=Collator Monitor
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=collator
WorkingDirectory=/home/collator/collator-monitor
ExecStart=/home/collator/collator-monitor/target/release/collator-monitor watch
Restart=always
RestartSec=10
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable collator-monitor
sudo systemctl start collator-monitor
```

### Docker

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/collator-monitor /usr/local/bin/
CMD ["collator-monitor", "watch"]
```

## Security Considerations

1. **Proxy Account**: The proxy seed has limited permissions (only collator-related calls), but should still be kept secure
2. **Never commit secrets**: Use environment variables or `.env` files (gitignored)
3. **Minimal permissions**: Configure the proxy with `Collator` type, not `Any` if possible
4. **Monitor logs**: Check for unexpected errors or unauthorized actions

## Troubleshooting

### "Failed to connect to chain"

- Check that the RPC URL is correct and accessible
- Try a different RPC endpoint
- Check your network/firewall settings

### "Transaction failed"

- Ensure the proxy is properly configured on-chain
- Check that the proxy account has funds for fees
- Verify session keys are registered

### "Insufficient funds"

- Top up your collator account
- The tool will notify you via Slack when this happens

## Contributing

Contributions are welcome! Please open an issue or PR.

## License

MIT License
