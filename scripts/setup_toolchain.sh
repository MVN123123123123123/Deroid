#!/usr/bin/env bash
# ==============================================================================
# setup_toolchain.sh - Configure Rust toolchain for ARM64 64KB page alignment
# ==============================================================================
set -euo pipefail

echo "[*] Configuring rustup toolchain for Universal Treble Linux..."

if ! command -v rustup >/dev/null 2>&1; then
    echo "[!] rustup not found. Please install rustup first."
    exit 1
fi

echo "[*] Adding target: aarch64-unknown-linux-gnu"
rustup target add aarch64-unknown-linux-gnu

echo "[*] Adding target: aarch64-unknown-linux-musl (optional static rescue)"
rustup target add aarch64-unknown-linux-musl

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

mkdir -p "${WORKSPACE_ROOT}/.cargo"

cat << 'EOF' > "${WORKSPACE_ROOT}/.cargo/config.toml"
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
rustflags = [
    "-C", "link-arg=-Wl,-z,max-page-size=65536",
]
EOF

echo "[+] Cargo configured with 64 KB ELF page alignment for Android 15+ kernels."
echo "[+] Toolchain setup complete!"
