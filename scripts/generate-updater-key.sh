#!/usr/bin/env bash
# Generate minisign key pair for dsh-desktop auto-updater.
#
# Usage:
#   ./generate-updater-key.sh
#
# This will generate:
#   - Private key: ~/.dsh-desktop-updater.key (keep secret!)
#   - Public key:  ~/.dsh-desktop-updater.key.pub (embed in app)
#
# After generation:
#   1. Copy the public key content to src/updater.rs (UPDATER_PUBKEY constant)
#   2. Add the private key content as GitHub Secret: DSH_UPDATER_PRIVATE_KEY
#   3. If private key has a password, add it as: DSH_UPDATER_PRIVATE_KEY_PASSWORD
#
# Requirements:
#   - rsign2: cargo install rsign2
#   - Or minisign: brew install minisign (macOS)

set -euo pipefail

KEY_DIR="$HOME"
PRIVATE_KEY="$KEY_DIR/.dsh-desktop-updater.key"
PUBLIC_KEY="$KEY_DIR/.dsh-desktop-updater.key.pub"

echo "=== dsh-desktop Updater Key Generator ==="
echo ""

# Check if rsign2 or minisign is available
if command -v rsign &> /dev/null; then
    TOOL="rsign"
elif command -v minisign &> /dev/null; then
    TOOL="minisign"
else
    echo "Error: Neither rsign nor minisign found."
    echo ""
    echo "Please install one of them:"
    echo "  - rsign2: cargo install rsign2"
    echo "  - minisign: brew install minisign (macOS)"
    echo "             or: https://github.com/jedisct1/minisign/releases"
    exit 1
fi

echo "Using tool: $TOOL"
echo ""

# Check if keys already exist
if [ -f "$PRIVATE_KEY" ]; then
    echo "Warning: Private key already exists at $PRIVATE_KEY"
    read -p "Overwrite? (y/N): " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        echo "Aborted."
        exit 0
    fi
    rm -f "$PRIVATE_KEY" "$PRIVATE_KEY.pub"
fi

# Generate key pair
echo "Generating key pair..."
if [ "$TOOL" = "rsign" ]; then
    rsign generate -s -p "$PUBLIC_KEY" -S "$PRIVATE_KEY"
else
    minisign -G -s "$PRIVATE_KEY" -p "$PUBLIC_KEY"
fi

echo ""
echo "=== Key pair generated ==="
echo ""
echo "Private key: $PRIVATE_KEY"
echo "Public key:  $PUBLIC_KEY"
echo ""

# Display public key content (for embedding in app)
echo "=== Public key content (copy to src/updater.rs): ==="
cat "$PUBLIC_KEY"
echo ""

# Display instructions
echo "=== Setup instructions ==="
echo ""
echo "1. Copy the public key content above to src/updater.rs:"
echo "   const UPDATER_PUBKEY: &str = \"<base64-content-from-public-key>\";"
echo ""
echo "2. Add the private key as GitHub Secret:"
echo "   Name:  DSH_UPDATER_PRIVATE_KEY"
echo "   Value: (contents of $PRIVATE_KEY)"
echo ""
echo "3. If you set a password for the private key, add it as:"
echo "   Name:  DSH_UPDATER_PRIVATE_KEY_PASSWORD"
echo "   Value: <your-password>"
echo ""
echo "4. Keep the private key secure! Never commit it to git."
echo ""
