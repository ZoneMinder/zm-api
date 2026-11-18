#!/usr/bin/env bash
# Build a zm-api .deb for the dev ZoneMinder box and install it over SSH.
#
# macOS cannot link FFmpeg/OpenSSL for Linux, so the package is built inside a
# Linux container whose distro + arch match the target box (Ubuntu 24.04 arm64).
# The resulting .deb carries versioned FFmpeg/OpenSSL deps, and apt resolves the
# matching runtime libs on the box. The shipped systemd unit runs passive (REST
# only) alongside stock ZoneMinder, and the postinst generates JWT keys.
#
# Usage:
#   ZM_SSH_USER=youruser ./scripts/deploy-dev.sh          # full .deb deploy
#   ZM_SSH_USER=youruser ./scripts/deploy-dev.sh --fast   # binary-only rsync
#
# Env (with defaults):
#   ZM_HOST=192.168.0.45        target box IP/hostname
#   ZM_SSH_USER=<required>      SSH user with passwordless sudo on the box
#   ARCH=arm64                  target arch (arm64|amd64)
#   DISTRO_IMAGE=ubuntu:24.04   build base; MUST match the box's distro+FFmpeg
#   SERVICE=zm-api              systemd unit name
set -euo pipefail

ZM_HOST="${ZM_HOST:-192.168.0.45}"
ARCH="${ARCH:-arm64}"
DISTRO_IMAGE="${DISTRO_IMAGE:-ubuntu:24.04}"
SERVICE="${SERVICE:-zm-api}"
FAST="no"
BUILD_ONLY="no"
for arg in "$@"; do
    case "$arg" in
        --fast) FAST="yes" ;;
        # Build the .deb locally and print the install commands; no SSH. Use
        # this when the box has no passwordless sudo — you run the install
        # step yourself in an interactive sudo session.
        --build-only) BUILD_ONLY="yes" ;;
    esac
done

# SSH user only required when we actually deploy over SSH.
if [ "$BUILD_ONLY" = "no" ]; then
    : "${ZM_SSH_USER:?set ZM_SSH_USER to an SSH user on $ZM_HOST (needs sudo; passwordless not required — an interactive prompt via ssh -t is used), or run with --build-only}"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SSH_TARGET="${ZM_SSH_USER:-user}@${ZM_HOST}"
PLATFORM="linux/${ARCH}"
BUILDER_IMAGE="zm-api-deb-builder:${DISTRO_IMAGE//[:\/]/-}-${ARCH}"

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; NC='\033[0m'
info() { echo -e "${GREEN}[deploy]${NC} $1"; }
warn() { echo -e "${YELLOW}[deploy]${NC} $1"; }
die()  { echo -e "${RED}[deploy]${NC} $1" >&2; exit 1; }

command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1 \
    || die "Docker is required to build the Linux package; start Docker Desktop."

# ---------------------------------------------------------------------------
# 1. Build (or reuse) a builder image with the FFmpeg/OpenSSL dev libs + Rust.
#    Cached as an image so only the first run pays the toolchain install cost.
# ---------------------------------------------------------------------------
if ! docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1; then
    info "Building builder image $BUILDER_IMAGE (first run only, a few minutes)..."
    docker build --platform "$PLATFORM" -t "$BUILDER_IMAGE" - <<DOCKERFILE
FROM ${DISTRO_IMAGE}
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        curl ca-certificates build-essential pkg-config libssl-dev \
        libavutil-dev libavformat-dev libavfilter-dev libavdevice-dev \
        libavcodec-dev libswscale-dev libswresample-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:\${PATH}"
RUN cargo install cargo-deb --locked
DOCKERFILE
else
    info "Reusing builder image $BUILDER_IMAGE"
fi

# Persist cargo registry + git caches between runs; target/ lives on the host
# (bind-mounted) so builds are incremental. Use a Linux-only target dir so it
# never clashes with the host macOS target/.
CARGO_CACHE_VOL="zm-api-cargo-cache"
docker volume inspect "$CARGO_CACHE_VOL" >/dev/null 2>&1 || docker volume create "$CARGO_CACHE_VOL" >/dev/null
LINUX_TARGET="target/deploy-${ARCH}"

run_builder() {
    docker run --rm --platform "$PLATFORM" \
        -v "$PROJECT_ROOT":/src -w /src \
        -v "${CARGO_CACHE_VOL}":/root/.cargo/registry \
        -e CARGO_TERM_COLOR=always \
        -e CARGO_TARGET_DIR="/src/${LINUX_TARGET}" \
        "$BUILDER_IMAGE" bash -euo pipefail -c "$1"
}

# Build the artifact (binary for --fast, else the .deb).
if [ "$FAST" = "yes" ]; then
    info "Fast build: compiling release binary in container..."
    run_builder "cargo build --release --bin zm-api"
    ARTIFACT="${PROJECT_ROOT}/${LINUX_TARGET}/release/zm-api"
    [ -f "$ARTIFACT" ] || die "binary not found at $ARTIFACT"
else
    info "Building .deb in container ($DISTRO_IMAGE / $ARCH)..."
    run_builder "cargo deb --output /src/${LINUX_TARGET}/zm-api.deb"
    ARTIFACT="${PROJECT_ROOT}/${LINUX_TARGET}/zm-api.deb"
    [ -f "$ARTIFACT" ] || die "no .deb produced at $ARTIFACT"
fi
info "Built $(basename "$ARTIFACT") ($(du -h "$ARTIFACT" | cut -f1))"

# On-box install command for whichever artifact we built. The upload goes to
# the login's home directory, not /tmp: with a fixed name in a directory every
# local user can write to, someone else on the box could put their own file
# there, or swap it between the upload and the `sudo install` below, and have
# it installed as root.
if [ "$FAST" = "yes" ]; then
    REMOTE="./zm-api.new"
    INSTALL_CMD="sudo install -m 0755 ${REMOTE} /usr/bin/zm-api && rm -f ${REMOTE} && sudo systemctl restart ${SERVICE} && systemctl --no-pager --full status ${SERVICE} | head -n 15"
else
    REMOTE="./zm-api.deb"
    INSTALL_CMD="sudo apt-get install -y ${REMOTE} && rm -f ${REMOTE} && sudo systemctl enable --now ${SERVICE} && sudo systemctl restart ${SERVICE} && systemctl --no-pager --full status ${SERVICE} | head -n 20"
fi

# --build-only: no SSH. Print exactly what to run on the box (for hosts without
# passwordless sudo, where you install manually in an interactive session).
if [ "$BUILD_ONLY" = "yes" ]; then
    info "Build-only: artifact ready. To install on the box, run:"
    echo
    echo "  scp \"$ARTIFACT\" ${ZM_HOST}:${REMOTE}"
    echo "  ssh -t <user>@${ZM_HOST} '${INSTALL_CMD}'"
    echo
    info "The 'ssh -t' allocates a TTY so sudo can prompt for your password."
    exit 0
fi

# Deploy over SSH. `ssh -t` gives sudo a TTY to prompt for the password, so
# passwordless sudo is not required.
info "Shipping to ${SSH_TARGET}:${REMOTE} ..."
scp "$ARTIFACT" "${SSH_TARGET}:${REMOTE}"
info "Installing on the box (sudo will prompt for your password)..."
ssh -t "$SSH_TARGET" "$INSTALL_CMD"

# ---------------------------------------------------------------------------
# Health check: the unit binds 0.0.0.0:8080 (settings/prod.toml).
# ---------------------------------------------------------------------------
info "Checking health endpoint on the box..."
if ssh "$SSH_TARGET" "curl -fsS http://127.0.0.1:8080/api/v3/server/health_check >/dev/null 2>&1"; then
    info "✅ ${SERVICE} is up and answering on :8080"
    info "   Reach it from here at: http://${ZM_HOST}:8080"
else
    warn "Service restarted but the health check did not pass yet."
    warn "Inspect with: ssh ${SSH_TARGET} 'journalctl -u ${SERVICE} -n 50 --no-pager'"
fi
