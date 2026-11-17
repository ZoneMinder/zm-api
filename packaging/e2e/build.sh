#!/usr/bin/env bash
#
# build.sh: build the zm-api package for one distro, inside that distro.
#
# The package is built in a container of the target distro so its dependency
# list names the FFmpeg and OpenSSL sonames that distro ships (cargo-deb's
# $auto and rpm's automatic Requires both read them from the build host).
# The working tree is packaged as it stands, uncommitted changes included, so
# a packaging edit can be tested before it is committed.
#
# Usage:
#   packaging/e2e/build.sh <distro>        # see distros.sh for names
#
# Output: target/e2e/<distro>/pkg/ holds the package(s).
#
# Env:
#   E2E_CACHE_DIR   host directory for the cargo registry and target caches.
#                   Unset, Docker volumes are used, which is much faster than
#                   a bind mount on macOS. CI sets it so actions/cache can
#                   save it.
#   E2E_BUILD_JOBS  cap on parallel compile jobs (CARGO_BUILD_JOBS).
#   CARGO_PROFILE_RELEASE_*
#                   forwarded to cargo. The optimised build of the zm-api crate
#                   peaks above 8 GB of compiler memory, which is all Docker
#                   Desktop gives its VM by default; on such a host set
#                   CARGO_PROFILE_RELEASE_OPT_LEVEL=0. The package then tests
#                   the same scripts and unit with a slower binary.
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# shellcheck source=packaging/e2e/distros.sh
. "$HERE/distros.sh"

distro="${1:?usage: build.sh <distro> (one of: $(e2e_distros))}"
e2e_distro "$distro"

arch="$(docker info --format '{{.Architecture}}')"
builder="zm-api-e2e-build:${distro}"
out="$ROOT/target/e2e/${distro}/pkg"
mkdir -p "$out"
rm -f "$out"/*

log() { printf '\033[0;32m[build %s]\033[0m %s\n' "$distro" "$*"; }

# ---------------------------------------------------------------------------
# Builder image: toolchain plus the FFmpeg/OpenSSL development libraries.
# Mirrors what .github/workflows/release.yml installs for each family.
# ---------------------------------------------------------------------------
builder_dockerfile() {
  echo "FROM ${E2E_IMAGE}"
  case "$E2E_FAMILY" in
    deb)
      cat <<'EOF'
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      curl ca-certificates git build-essential pkg-config clang libclang-dev \
      libssl-dev libavutil-dev libavformat-dev libavfilter-dev \
      libavdevice-dev libavcodec-dev libswscale-dev libswresample-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo install cargo-deb --locked --root /usr/local
EOF
      ;;
    rpm)
      case "$distro" in
        rocky-*|alma-*|rhel-*)
          # FFmpeg is not in EL or EPEL: RPM Fusion carries it, and needs CRB.
          cat <<'EOF'
RUN dnf -y install epel-release dnf-plugins-core \
    && dnf config-manager --set-enabled crb \
    && dnf -y install --nogpgcheck \
         "https://mirrors.rpmfusion.org/free/el/rpmfusion-free-release-$(rpm -E %rhel).noarch.rpm"
EOF
          install="dnf -y install --allowerasing" ;;
        opensuse-*) install="zypper --non-interactive install" ;;
        *)          install="dnf -y install" ;;
      esac
      # openSUSE: pkgconfig(openssl) can resolve to LibreSSL; see the spec.
      case "$distro" in
        opensuse-*) openssl_dev=libopenssl-devel ;;
        *)          openssl_dev="'pkgconfig(openssl)'" ;;
      esac
      cat <<EOF
RUN ${install} \\
      git tar gzip rpm-build rust cargo gcc systemd-rpm-macros \\
      clang clang-devel \\
      ${openssl_dev} \\
      'pkgconfig(libavcodec)' 'pkgconfig(libavformat)' \\
      'pkgconfig(libavutil)' 'pkgconfig(libavfilter)' \\
      'pkgconfig(libavdevice)' 'pkgconfig(libswscale)' \\
      'pkgconfig(libswresample)'
EOF
      case "$distro" in
        rocky-*|alma-*|rhel-*)
          # EL9's rust-toolset (1.92) is below the crate's rust-version (1.94,
          # needed by sea-orm 2 and sqlx 0.9), so build with current stable
          # from rustup, as the Debian builders do. The distro rust stays
          # installed to satisfy the spec's BuildRequires.
          cat <<'EOF'
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
EOF
          ;;
      esac
      ;;
    arch)
      cat <<'EOF'
RUN pacman -Syu --noconfirm base-devel git rust ffmpeg openssl clang \
    && useradd -m builder
EOF
      ;;
  esac
}

if ! docker image inspect "$builder" >/dev/null 2>&1; then
  log "building builder image $builder"
  builder_dockerfile | docker build -t "$builder" -
else
  log "reusing builder image $builder"
fi

# ---------------------------------------------------------------------------
# Caches. A cold release build is 10 to 20 minutes, a warm one under a minute.
# ---------------------------------------------------------------------------
if [[ -n "${E2E_CACHE_DIR:-}" ]]; then
  mkdir -p "$E2E_CACHE_DIR/registry" "$E2E_CACHE_DIR/target-${distro}"
  reg_mount="$E2E_CACHE_DIR/registry"
  target_mount="$E2E_CACHE_DIR/target-${distro}"
else
  reg_mount="zm-api-e2e-cargo-registry"
  target_mount="zm-api-e2e-target-${distro}-${arch}"
fi

# The source tree as it stands: tracked files plus untracked ones that are not
# ignored. `git archive HEAD` would drop the edit under test.
src_tar="$ROOT/target/e2e/${distro}/src.tar"
(cd "$ROOT" && git ls-files -co --exclude-standard -z | tar --null -T - -cf "$src_tar")

# Any CARGO_PROFILE_RELEASE_* override in the caller's environment.
profile_env=()
while IFS= read -r name; do
  profile_env+=(-e "$name")
done < <(compgen -e | grep '^CARGO_PROFILE_RELEASE_' || true)
# An empty array is an unbound variable under `set -u` before bash 4.4.
[[ ${#profile_env[@]} -gt 0 ]] || profile_env=(-e E2E_NO_PROFILE_OVERRIDE=1)

run_builder() {
  docker run --rm \
    -v "$src_tar":/src.tar:ro \
    -v "$out":/out \
    -v "$reg_mount":/cargo-registry \
    -v "$target_mount":/cargo-target \
    -e CARGO_TERM_COLOR=always \
    -e CARGO_TARGET_DIR=/cargo-target \
    ${E2E_BUILD_JOBS:+-e CARGO_BUILD_JOBS="$E2E_BUILD_JOBS"} \
    "${profile_env[@]}" \
    -e E2E_UID="$(id -u)" -e E2E_GID="$(id -g)" \
    "$builder" bash -euo pipefail -c "$1"
}

# Common preamble: unpack the source and point cargo's registry at the cache.
prep='
  mkdir -p /src && tar -xf /src.tar -C /src && cd /src
  home="${CARGO_HOME:-$HOME/.cargo}"; mkdir -p "$home"
  rm -rf "$home/registry" && ln -s /cargo-registry "$home/registry"
'

log "building ${E2E_FAMILY} package ($arch)"
case "$E2E_FAMILY" in
  deb)
    # Every bin target first: the package ships `migrator` as zm-api-db and
    # cargo-deb only copies paths.
    run_builder "$prep"'
      cargo build --release --bins --locked
      cargo deb --no-build --output /out/
      chown "$E2E_UID:$E2E_GID" /out/*
    '
    ;;
  rpm)
    run_builder "$prep"'
      version="$(sed -n "s/^Version:[[:space:]]*//p" packaging/rpm/zm-api.spec | head -1)"
      topdir="$(rpm --eval %_topdir)"
      mkdir -p "$topdir/SOURCES"
      tar -czf "$topdir/SOURCES/zm-api-${version}.tar.gz" \
        --transform "s,^\./,zm-api-${version}/," -C /src .
      # Fedora and EL export RUSTFLAGS in %build (opt-level 3, one codegen
      # unit, full debuginfo), which overrides any cargo profile setting. An
      # opt-level asked for through CARGO_PROFILE_RELEASE_OPT_LEVEL has to
      # reach rustc through the macro those flags come from. That drops the
      # debuginfo the debug subpackages are made of, so they are turned off
      # as well. Neither applies to a normal build.
      defines=()
      if [[ -n "${CARGO_PROFILE_RELEASE_OPT_LEVEL:-}" ]]; then
        defines=(--define "build_rustflags -Copt-level=${CARGO_PROFILE_RELEASE_OPT_LEVEL}"
                 --define "debug_package %{nil}")
      fi
      rpmbuild -bb "${defines[@]}" packaging/rpm/zm-api.spec
      find "$topdir/RPMS" -name "zm-api-[0-9]*.rpm" -exec cp {} /out/ \;
      chown "$E2E_UID:$E2E_GID" /out/*
    '
    ;;
  arch)
    # makepkg refuses root. Pre-stage the tarball it would otherwise download,
    # as release.yml does, so it builds this tree and not the published tag.
    run_builder "$prep"'
      mkdir /build && cp -r packaging/arch/. /build/
      pkgver="$(sed -n "s/^pkgver=//p" /build/PKGBUILD | head -1)"
      pkgtag="$(sed -n "s/^_pkgtag=//p" /build/PKGBUILD | head -1)"
      tar -czf "/build/zm-api-${pkgver}.tar.gz" \
        --transform "s,^\./,zm-api-${pkgtag#v}/," -C /src .
      chown -R builder:builder /build /cargo-target /cargo-registry
      runuser -u builder -- bash -c "
        mkdir -p ~/.cargo && ln -sfn /cargo-registry ~/.cargo/registry
        cd /build && CARGO_TARGET_DIR=/cargo-target makepkg -d --noconfirm"
      cp /build/*.pkg.tar.zst /out/
      chown "$E2E_UID:$E2E_GID" /out/*
    '
    ;;
esac

log "built:"
ls -l "$out"
