#!/bin/bash
# VPS setup script for poly-arb-bot
# Run on a fresh Ubuntu 22.04+ VPS (DigitalOcean, Hetzner, etc.)
# Usage: bash setup.sh

set -e

echo "═══════════════════════════════════════════"
echo "  Polymarket Arb Bot — VPS Setup"
echo "═══════════════════════════════════════════"

# Install Rust if not present
if ! command -v cargo &> /dev/null; then
    echo "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

echo "Rust version: $(rustc --version)"

# Build
echo "Building (this takes 2-3 minutes the first time)..."
cargo build --release

echo ""
echo "═══════════════════════════════════════════"
echo "  Build complete!"
echo ""
echo "  Next steps:"
echo "  1. export POLYMARKET_PRIVATE_KEY=\"0xYOUR_KEY\""
echo "  2. cargo run --release"
echo ""
echo "  Or run in background with tmux:"
echo "  tmux new -s polybot"
echo "  export POLYMARKET_PRIVATE_KEY=\"0x...\""
echo "  cargo run --release"
echo "  (Ctrl+B, D to detach)"
echo "═══════════════════════════════════════════"
