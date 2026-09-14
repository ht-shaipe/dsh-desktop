#!/usr/bin/env bash
# Sign update packages for dsh-desktop auto-updater.
#
# Usage:
#   ./sign-update.sh <file-to-sign>
#
# This will create a .sig file alongside the signed file.
# The private key is read from environment variable or file.
#
# Environment variables:
#   DSH_UPDATER_PRIVATE_KEY         - Private key content (or path to key file)
#   DSH_UPDATER_PRIVATE_KEY_PASSWORD - Private key password (optional)
#
# Requirements:
#   - rsign2: cargo install rsign2
#   - Or minisign: brew install minisign (macOS)

set -euo pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <file-to-sign>"
    echo ""
    echo "Example:"
    echo "  $0 dsh-desktop-macos-aarch64.dmg"
    echo "  $0 dsh-desktop-linux-x86_64.tar.gz"
    exit 1
fi

FILE_TO_SIGN="$1"

if [ ! -f "$FILE_TO_SIGN" ]; then
    echo "Error: File not found: $FILE_TO_SIGN"
    exit 1
fi

# Check if rsign or minisign is available
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
    exit 1
fi

# Get private key
PRIVATE_KEY="${DSH_UPDATER_PRIVATE_KEY:-}"
PRIVATE_KEY_PASSWORD="${DSH_UPDATER_PRIVATE_KEY_PASSWORD:-}"

if [ -z "$PRIVATE_KEY" ]; then
    # Try to read from default location
    DEFAULT_KEY="$HOME/.dsh-desktop-updater.key"
    if [ -f "$DEFAULT_KEY" ]; then
        PRIVATE_KEY="$DEFAULT_KEY"
    else
        echo "Error: No private key found."
        echo ""
        echo "Set the private key via:"
        echo "  - Environment variable: DSH_UPDATER_PRIVATE_KEY"
        echo "  - Or place key at: $DEFAULT_KEY"
        exit 1
    fi
fi

# Create temp key file if PRIVATE_KEY is content (not path)
TEMP_KEY=""
if [[ "$PRIVATE_KEY" == *"-----BEGIN"* ]]; then
    TEMP_KEY=$(mktemp)
    echo "$PRIVATE_KEY" > "$TEMP_KEY"
    PRIVATE_KEY="$TEMP_KEY"
fi

echo "Signing: $FILE_TO_SIGN"
echo "Tool: $TOOL"
echo ""

# Sign the file
SIGN_ARGS=()
if [ "$TOOL" = "rsign" ]; then
    SIGN_ARGS+=("-s" "$PRIVATE_KEY")
    SIGN_ARGS+=("-p" "${PRIVATE_KEY}.pub" 2>/dev/null || true)
    if [ -n "$PRIVATE_KEY_PASSWORD" ]; then
        SIGN_ARGS+=("-P" "$PRIVATE_KEY_PASSWORD")
    fi
    SIGN_ARGS+=("$FILE_TO_SIGN")
    rsign sign "${SIGN_ARGS[@]}"
else
    SIGN_ARGS+=("-s" "$PRIVATE_KEY")
    SIGN_ARGS+=("-m" "$FILE_TO_SIGN")
    if [ -n "$PRIVATE_KEY_PASSWORD" ]; then
        SIGN_ARGS+=("-P" "$PRIVATE_KEY_PASSWORD")
    fi
    minisign -S "${SIGN_ARGS[@]}"
fi

# Cleanup temp key
if [ -n "$TEMP_KEY" ]; then
    rm -f "$TEMP_KEY"
fi

# Check if signature was created
SIG_FILE="${FILE_TO_SIGN}.sig"
if [ -f "$SIG_FILE" ]; then
    echo ""
    echo "=== Signature created ==="
    echo "File: $SIG_FILE"
    echo ""
    echo "Signature content:"
    cat "$SIG_FILE"
else
    echo "Error: Signature file not created."
    exit 1
fi
