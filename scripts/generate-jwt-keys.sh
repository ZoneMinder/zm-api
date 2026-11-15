#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/generate-jwt-keys.sh [key_dir]

Generates JWT RSA key pairs for access/refresh tokens.
Defaults to static/key (override with JWT_KEY_DIR or the first argument).

Options:
  --force   overwrite existing keys
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

force="0"
if [[ "${1:-}" == "--force" ]]; then
  force="1"
  shift
fi

KEY_DIR="${1:-${JWT_KEY_DIR:-static/key}}"

if ! command -v openssl >/dev/null 2>&1; then
  echo "openssl is required to generate JWT keys" >&2
  exit 1
fi

mkdir -p "$KEY_DIR"

generate_pair() {
  local name="$1"
  local private_key="$KEY_DIR/private_${name}_rsa_key.pem"
  local public_key="$KEY_DIR/public_${name}_rsa_key.pem"

  if [[ -f "$private_key" && -f "$public_key" && "$force" != "1" ]]; then
    echo "Keys exist for ${name}, skipping (use --force to regenerate)"
    return 0
  fi

  # umask first: without it the key is world-readable until the chmod below.
  (umask 077 && openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$private_key")
  openssl rsa -in "$private_key" -pubout -out "$public_key"
  chmod 600 "$private_key"
  chmod 644 "$public_key"
}

generate_pair "access"
generate_pair "refresh"

echo "JWT keys ready in $KEY_DIR"
