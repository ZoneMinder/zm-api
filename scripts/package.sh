#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: ./scripts/package.sh [deb|rpm|arch|all]

Builds distribution packages for zm-api.

  deb   Debian/Ubuntu .deb via cargo-deb (built here when tooling is present).
  rpm   Fedora/RHEL/openSUSE .rpm from packaging/rpm/zm-api.spec via rpmbuild.
  arch  Arch PKGBUILD via makepkg.
  all   deb plus whatever rpm/arch tooling is available.

All packages ship in PASSIVE mode (REST API only). Use `zm-api-takeover` on the
target host to hand ZoneMinder daemon supervision to zm-api.
EOF
}

target="${1:-all}"
case "$target" in
  -h|--help) usage; exit 0 ;;
  deb|rpm|arch|all) ;;
  *) echo "Unknown target: $target" >&2; usage >&2; exit 1 ;;
esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"

build_deb() {
  if ! command -v cargo-deb >/dev/null 2>&1; then
    echo "cargo-deb is required: cargo install cargo-deb" >&2
    return 1
  fi
  (cd "$ROOT" && cargo deb --locked)
}

build_rpm() {
  if ! command -v rpmbuild >/dev/null 2>&1; then
    echo "rpmbuild not found. Build on a Fedora/EL/openSUSE host, or submit" >&2
    echo "packaging/rpm/zm-api.spec to COPR (Fedora) / OBS (openSUSE)." >&2
    return 1
  fi
  # The spec's %autosetup uses the upstream Version (no pre-release suffix), so
  # the tarball name/prefix must match that, not Cargo's full "3.0.0-alpha.1".
  local topdir tarball upstream
  upstream="${VERSION%%-*}"
  topdir="$(rpm --eval %_topdir)"
  mkdir -p "$topdir/SOURCES"
  tarball="$topdir/SOURCES/zm-api-${upstream}.tar.gz"
  echo "Creating source tarball $tarball"
  (cd "$ROOT" && git archive --format=tar.gz --prefix="zm-api-${upstream}/" -o "$tarball" HEAD)
  rpmbuild -bb "$ROOT/packaging/rpm/zm-api.spec"
}

build_arch() {
  if ! command -v makepkg >/dev/null 2>&1; then
    echo "makepkg not found. Build on an Arch host with packaging/arch/PKGBUILD," >&2
    echo "or publish it to the AUR." >&2
    return 1
  fi
  (cd "$ROOT/packaging/arch" && makepkg -f)
}

rc=0
case "$target" in
  deb)  build_deb  || rc=$? ;;
  rpm)  build_rpm  || rc=$? ;;
  arch) build_arch || rc=$? ;;
  all)
    build_deb  || rc=$?
    build_rpm  || true   # optional: only when rpm tooling is present
    build_arch || true   # optional: only when arch tooling is present
    ;;
esac
exit "$rc"
