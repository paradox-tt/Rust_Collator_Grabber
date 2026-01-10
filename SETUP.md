# Collator Monitor - Secure Service Setup Guide

This guide explains how to set up the Collator Monitor as a secure systemd service on Linux.

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Installation](#installation)
3. [Secure Configuration](#secure-configuration)
4. [Systemd Service Setup](#systemd-service-setup)
5. [Managing the Service](#managing-the-service)
6. [Monitoring & Logs](#monitoring--logs)
7. [Security Best Practices](#security-best-practices)
8. [Troubleshooting](#troubleshooting)

---

## Prerequisites

- Linux server (Ubuntu 22.04+ / Debian 12+ recommended)
- Rust 1.83+ installed
- sudo/root access for service installation
- Slack webhook URL (optional, for notifications)

## Installation

### 1. Create a dedicated user

Running as a dedicated non-root user improves security:

```bash
# Create system user (no login shell, no home directory login)
sudo useradd --system --shell /usr/sbin/nologin --create-home --home-dir /home/collator-registrar collator-monitor
```

### 2. Build the application

```bash
# Clone or copy the source to a build directory
cd /tmp
unzip collator-monitor.zip
cd collator-monitor

# Build release binary
cargo build --release

# Copy binary to installation directory
sudo mkdir -p /home/collator-registrar/bin
sudo cp target/release/collator-monitor /home/collator-registrar/bin/
sudo chown -R collator-monitor:collator-monitor /home/collator-registrar
```

### 3. Set permissions on binary

```bash
sudo chmod 750 /home/collator-registrar/bin/collator-monitor
```

---

## Secure Configuration

The `.env` file contains sensitive credentials (proxy seed phrase). We'll protect it carefully.

### 1. Create the environment file

```bash
# Create config directory with restricted access
sudo mkdir -p /home/collator-registrar/config
sudo chown collator-registrar:collator-registrar /home/collator-registrar/config
sudo chmod 700 /home/collator-registrar/config

# Create the .env file
sudo -u collator-registrar nano /home/collator-registrar/config/.env
```

### 2. Add your configuration

```bash
# /home/collator-registrar/config/.env

# Collator addresses (public - these appear on-chain anyway)
COLLATOR_POLKADOT_COLLATOR_ADDRESS=1YourPolkadotCollatorAddress...
COLLATOR_KUSAMA_COLLATOR_ADDRESS=CYourKusamaCollatorAddress...

# SENSITIVE: Proxy account seed phrase
# This account should ONLY have NonTransfer proxy rights
# It cannot transfer funds, only perform collator operations
COLLATOR_PROXY_SEED=your twelve or twenty four word mnemonic phrase here

# Slack webhook for notifications (optional)
COLLATOR_SLACK_WEBHOOK_URL=https://hooks.slack.com/services/YOUR/WEBHOOK/URL

# Check interval in seconds (default: 3600 = 1 hour)
COLLATOR_CHECK_INTERVAL_SECS=3600
```

### 3. Secure the .env file

```bash
# Only the collator-monitor user can read this file
sudo chmod 600 /home/collator-registrar/config/.env
sudo chown collator-registrar:collator-registrar /home/collator-registrar/config/.env

# Verify permissions
ls -la /home/collator-registrar/config/.env
# Should show: -rw------- 1 collator-monitor collator-monitor
```

### 4. (Optional) Create config.toml for chain-specific settings

```bash
sudo -u collator-monitor nano /home/collator-registrar/config/config.toml
```

```toml
# /home/collator-registrar/config/config.toml
# Chain-specific overrides (optional)

[chains.polkadot_bridgehub]
enabled = true  # Check status but no auto-actions

[chains.kusama_bridgehub]
enabled = true  # Check status but no auto-actions
```

---

## Systemd Service Setup

### 1. Create the service file

```bash
sudo nano /etc/systemd/system/collator-monitor.service
```

```ini
[Unit]
Description=Collator Monitor - Automatic collator re-registration service
Documentation=https://github.com/example/collator-monitor
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=collator-monitor
Group=collator-monitor

# Working directory
WorkingDirectory=/home/collator-registrar

# Environment file with secrets
EnvironmentFile=/home/collator-registrar/config/.env

# Run in watch mode with 1-hour intervals
ExecStart=/home/collator-registrar/bin/collator-monitor watch --interval 3600

# Restart policy
Restart=always
RestartSec=30

# Security hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
MemoryDenyWriteExecute=true
LockPersonality=true

# Allow network access (required for RPC connections)
PrivateNetwork=false

# Allow reading config directory
ReadOnlyPaths=/
ReadWritePaths=/home/collator-registrar

# Limit resources (adjust as needed)
MemoryMax=512M
CPUQuota=50%

# Logging
StandardOutput=journal
StandardError=journal
SyslogIdentifier=collator-monitor

[Install]
WantedBy=multi-user.target
```

### 2. Reload systemd and enable the service

```bash
# Reload systemd to recognize new service
sudo systemctl daemon-reload

# Enable service to start on boot
sudo systemctl enable collator-monitor

# Start the service
sudo systemctl start collator-monitor

# Check status
sudo systemctl status collator-monitor
```

---

## Managing the Service

### Common commands

```bash
# Start the service
sudo systemctl start collator-monitor

# Stop the service
sudo systemctl stop collator-monitor

# Restart the service
sudo systemctl restart collator-monitor

# Check status
sudo systemctl status collator-monitor

# Disable auto-start on boot
sudo systemctl disable collator-monitor
```

### Run a one-time check (for testing)

```bash
# Run as the service user with the environment file
sudo -u collator-monitor bash -c 'source /home/collator-registrar/config/.env && /home/collator-registrar/bin/collator-monitor check'

# Or just check status (read-only)
sudo -u collator-monitor bash -c 'source /home/collator-registrar/config/.env && /home/collator-registrar/bin/collator-monitor status'
```

---

## Monitoring & Logs

### View logs

```bash
# Follow logs in real-time
sudo journalctl -u collator-monitor -f

# View last 100 lines
sudo journalctl -u collator-monitor -n 100

# View logs from today
sudo journalctl -u collator-monitor --since today

# View logs with timestamps
sudo journalctl -u collator-monitor -o short-precise
```

### Enable debug logging

Edit the service file to add debug logging:

```bash
sudo systemctl edit collator-monitor
```

Add:
```ini
[Service]
Environment="RUST_LOG=debug"
```

Then restart:
```bash
sudo systemctl restart collator-monitor
```

---

## Security Best Practices

### 1. Proxy Account Security

The proxy seed phrase is the most sensitive piece of configuration. Mitigate risk by:

- **Use NonTransfer proxy type**: The proxy account can only perform collator operations, NOT transfer funds
- **Minimal balance on proxy**: Keep only enough for transaction fees (~0.1 DOT/KSM)
- **Separate proxy per environment**: Don't reuse the same proxy for testnet/mainnet

### 2. File Permissions Summary

```
/home/collator-registrar/
├── bin/
│   └── collator-monitor     # 750 collator-monitor:collator-monitor
├── config/
│   ├── .env                 # 600 collator-monitor:collator-monitor (SECRETS)
│   └── config.toml          # 640 collator-monitor:collator-monitor
```

### 3. Firewall Rules

The service only needs outbound HTTPS/WSS connections:

```bash
# If using ufw
sudo ufw allow out 443/tcp comment "HTTPS/WSS for RPC"

# The service doesn't need any inbound ports
```

### 4. Secrets Rotation

If you suspect the proxy seed is compromised:

1. Remove the proxy relationship on-chain immediately
2. Create a new proxy account
3. Update `.env` with new seed
4. Restart the service

### 5. Audit Access

```bash
# Check who has accessed the config
sudo ausearch -f /home/collator-registrar/config/.env

# (Requires auditd to be configured)
```

---

## Troubleshooting

### Service won't start

```bash
# Check for syntax errors in service file
sudo systemd-analyze verify collator-monitor.service

# Check detailed status
sudo systemctl status collator-monitor -l

# Check recent logs
sudo journalctl -u collator-monitor -n 50 --no-pager
```

### Permission denied errors

```bash
# Verify file ownership
ls -la /home/collator-registrar/
ls -la /home/collator-registrar/config/

# Fix permissions if needed
sudo chown -R collator-monitor:collator-monitor /home/collator-registrar
sudo chmod 600 /home/collator-registrar/config/.env
```

### Connection errors

```bash
# Test RPC connectivity manually
curl -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"system_health","params":[],"id":1}' \
  https://rpc-asset-hub-polkadot.luckyfriday.io

# Check if DNS resolution works
nslookup rpc-asset-hub-polkadot.luckyfriday.io
```

### Slack notifications not working

```bash
# Test webhook manually
curl -X POST -H 'Content-type: application/json' \
  --data '{"text":"Test message from collator-monitor"}' \
  YOUR_WEBHOOK_URL
```

---

## Notification Behavior

The service uses smart rate limiting to avoid flooding Slack:

| Notification Type | Rate Limit | Notes |
|-------------------|------------|-------|
| ✅ Registration Success | None | Always sent immediately |
| 📈 Bond Updated | None | Always sent immediately |
| ⚠️ Insufficient Funds | 4 hours | Rate limited per chain |
| ⚠️ Cannot Compete | 4 hours | Rate limited per chain |
| 🔧 Manual Action Required | 4 hours | Rate limited per chain |
| ❌ Error | 4 hours | Rate limited per chain |

When a success notification is sent (registration or bond update), all rate limits for that chain are cleared, allowing immediate alerts if issues recur.

---

## Updating the Service

```bash
# Stop the service
sudo systemctl stop collator-monitor

# Build new version
cd /path/to/source
cargo build --release

# Replace binary
sudo cp target/release/collator-monitor /home/collator-registrar/bin/
sudo chown collator-monitor:collator-monitor /home/collator-registrar/bin/collator-monitor

# Start the service
sudo systemctl start collator-monitor

# Verify it's running
sudo systemctl status collator-monitor
```
