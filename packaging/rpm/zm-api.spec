# RPM spec for zm-api.
#
# CI builds and tests this on Fedora 41 and openSUSE Tumbleweed only. The spec
# has %if branches for the RHEL family and it is expected to work there, but
# that is unverified — build it yourself before relying on it.
# Build locally with:  rpmbuild -bb packaging/rpm/zm-api.spec  (after placing a
# source tarball in ~/rpmbuild/SOURCES), or submit to COPR / openSUSE OBS.

# Package name is hyphenated (distro convention, matches the .deb and the repo).
# The Rust crate/binary, systemd unit and install paths stay underscored
# (zm-api) — %{appname} carries that everywhere paths/files are referenced.
%global appname zm-api

# Rust debuginfo refers to cargo's registry and the build tree, so
# find-debuginfo leaves debugsourcefiles.list empty and rpm on EL9 fails the
# build on it ("Empty %files file"). The debuginfo subpackage is unaffected.
%undefine _debugsource_packages

Name:           zm-api
Version:        3.0.0
# Pre-release ordering: 0.N.prerel sorts before the stable release.
# For the stable 3.0.0, set Release back to 1 (plus the dist tag).
Release:        0.1.alpha1%{?dist}
Summary:        ZoneMinder REST API and daemon supervisor

License:        AGPL-3.0-or-later
URL:            https://github.com/SteveGilvarry/zm-api
Source0:        %{appname}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  systemd-rpm-macros
# libclang for bindgen (clang-sys), pulled in via the FFmpeg bindings build.
BuildRequires:  clang-devel
# FFmpeg + OpenSSL development libraries, requested via pkg-config virtual
# provides so the right package resolves on each distro (Fedora's *-free-devel,
# openSUSE's libav*-devel / Packman, etc.). ffmpeg-sys-next links these.
BuildRequires:  pkgconfig(libavcodec)
BuildRequires:  pkgconfig(libavformat)
BuildRequires:  pkgconfig(libavutil)
BuildRequires:  pkgconfig(libavfilter)
BuildRequires:  pkgconfig(libavdevice)
BuildRequires:  pkgconfig(libswscale)
BuildRequires:  pkgconfig(libswresample)
# openSUSE has three pkgconfig(openssl) providers (OpenSSL, LibreSSL, AWS-LC)
# and zypper may pick LibreSSL, which openssl-sys rejects past 4.3. Ask for
# OpenSSL by name there.
%if 0%{?suse_version}
BuildRequires:  libopenssl-devel
%else
BuildRequires:  pkgconfig(openssl)
%endif

# useradd/groupadd: shadow-utils on Fedora/EL, shadow on openSUSE.
# pgrep, used by zm-api-takeover: procps-ng on Fedora/EL, procps on openSUSE.
%if 0%{?suse_version}
Requires:       shadow
Requires:       procps
%else
Requires:       shadow-utils
Requires:       procps-ng
%endif
Requires:       openssl
# runuser, used by zm-api-takeover's pre-flight checks.
Requires:       util-linux
Requires(post): systemd
Requires(preun): systemd
Requires(postun): systemd

%description
zm-api is a Rust REST API for managing a ZoneMinder installation. It can run
passively alongside stock ZoneMinder (REST API only) or, after disabling
zoneminder.service, take over supervision of the ZoneMinder daemons.

%prep
%autosetup -n %{appname}-%{version}

%build
cargo build --release --locked

%install
# CARGO_TARGET_DIR lets a CI or test build keep its cache outside the build
# tree, which rpmbuild recreates from the tarball every time.
target="${CARGO_TARGET_DIR:-target}"
install -D -m 0755 "$target/release/%{appname}"        %{buildroot}%{_bindir}/%{appname}
# Built as `migrator`; installed prefixed, that name is too generic for /usr/bin.
install -D -m 0755 "$target/release/migrator"          %{buildroot}%{_bindir}/%{appname}-db
install -D -m 0755 packaging/zm-api-takeover.sh        %{buildroot}%{_bindir}/%{appname}-takeover
install -D -m 0755 packaging/setup-instance.sh         %{buildroot}%{_datadir}/%{appname}/setup-instance.sh
install -D -m 0644 settings/base.toml                  %{buildroot}%{_sysconfdir}/%{appname}/base.toml
install -D -m 0644 settings/prod.toml                  %{buildroot}%{_sysconfdir}/%{appname}/prod.toml
install -D -m 0644 packaging/systemd/zm-api.env        %{buildroot}%{_sysconfdir}/%{appname}/zm-api.env
install -D -m 0644 packaging/systemd/zm-api.service    %{buildroot}%{_unitdir}/%{appname}.service
install -d -m 0755 %{buildroot}%{_sysconfdir}/systemd/system/%{appname}.service.d
install -D -m 0644 packaging/man/zm-api.8              %{buildroot}%{_mandir}/man8/%{appname}.8
install -D -m 0644 packaging/man/zm-api-takeover.8     %{buildroot}%{_mandir}/man8/%{appname}-takeover.8
install -D -m 0644 packaging/man/zm-api-db.8           %{buildroot}%{_mandir}/man8/%{appname}-db.8
install -D -m 0644 packaging/man/zm-api.env.5          %{buildroot}%{_mandir}/man5/zm-api.env.5
# NOTE: static/ is not packaged — it holds dev JWT keys. Per-install keys are
# generated into /var/lib/zm-api/keys by setup-instance.sh.

%post
# Provision the service account drop-in, dirs and JWT keys (idempotent), then
# register the unit. Ships in passive mode, so this never disturbs a running
# ZoneMinder.
%{_datadir}/%{appname}/setup-instance.sh \
  || echo "zm-api: setup-instance.sh failed; fix the cause and re-run it before starting zm-api" >&2
%systemd_post %{appname}.service
if [ $1 -eq 1 ] && [ -d /run/systemd/system ]; then
  systemctl enable --now %{appname}.service || :
fi

%preun
# On removal (not upgrade) in takeover mode, zm-api is the only thing running
# the capture daemons. Hand control back so the host keeps recording.
if [ $1 -eq 0 ] && [ -d /run/systemd/system ] \
    && grep -qE '^APP_DAEMON__ENABLED=true' %{_sysconfdir}/%{appname}/zm-api.env 2>/dev/null; then
  %{_bindir}/%{appname}-takeover --revert --yes || :
fi
%systemd_preun %{appname}.service

%postun
%systemd_postun_with_restart %{appname}.service
# On removal (not upgrade), take the generated account drop-in away too.
if [ $1 -eq 0 ]; then
  rm -f %{_sysconfdir}/systemd/system/%{appname}.service.d/10-account.conf
  rmdir %{_sysconfdir}/systemd/system/%{appname}.service.d 2>/dev/null || :
  [ -d /run/systemd/system ] && systemctl daemon-reload || :
fi

%files
%license LICENSE
%{_bindir}/%{appname}
%{_bindir}/%{appname}-db
%{_bindir}/%{appname}-takeover
%{_mandir}/man8/%{appname}.8*
%{_mandir}/man8/%{appname}-takeover.8*
%{_mandir}/man8/%{appname}-db.8*
%{_mandir}/man5/zm-api.env.5*
%dir %{_datadir}/%{appname}
%{_datadir}/%{appname}/setup-instance.sh
%{_unitdir}/%{appname}.service
# Written by setup-instance.sh in %%post, removed in %%postun.
%dir %{_sysconfdir}/systemd/system/%{appname}.service.d
%ghost %{_sysconfdir}/systemd/system/%{appname}.service.d/10-account.conf
%dir %{_sysconfdir}/%{appname}
%config(noreplace) %{_sysconfdir}/%{appname}/base.toml
%config(noreplace) %{_sysconfdir}/%{appname}/prod.toml
%config(noreplace) %{_sysconfdir}/%{appname}/zm-api.env

%changelog
* Sun May 31 2026 Steve Gilvarry <SteveGilvarry@users.noreply.github.com> - 3.0.0-0.1.alpha1
- First Rust release (3.0.0-alpha.1). Passive by default; zm-api-takeover for
  daemon control. Initial RPM packaging.
