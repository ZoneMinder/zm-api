#!/usr/bin/env bash
#
# setup-instance.sh: idempotent provisioning for a zm-api install.
#
# Installed to /usr/share/zm-api/setup-instance.sh and run by the package
# post-install scripts (deb postinst, rpm %post, Arch .install) on every
# install and upgrade. Safe to run by hand at any time, and worth doing after
# ZoneMinder is installed, removed, or moved to a different web server:
#
#   sudo /usr/share/zm-api/setup-instance.sh && sudo systemctl restart zm-api
#
# It does not enable or start the service and does not touch ZoneMinder's
# daemons. zm-api ships in passive mode (see zm-api.env).
#
# The service account
# -------------------
# zm-api runs as the account ZoneMinder runs as. ZoneMinder has no account of
# its own: its daemons run as the web server's user, which its packaging picks
# per distribution and records in zm.conf as ZM_WEB_USER and ZM_WEB_GROUP:
#
#   Debian, Ubuntu         www-data:www-data
#   Fedora, RHEL family    apache:apache   (nginx:nginx on the nginx variant)
#   openSUSE               wwwrun:www
#   Arch                   http:http
#
# Everything ZoneMinder owns is owned by that account and mostly closed to
# anyone else: zm.conf is mode 0640, the events tree and the socket directory
# are 0755. A separate account can read none of the database credentials and,
# after takeover, write none of the recordings. Sharing the account also makes
# takeover and revert symmetric, because zmc creates the same files with the
# same owner whichever supervisor started it.
#
# Without ZoneMinder on the host there is no such account, and zm-api gets a
# dedicated `zoneminder` one.
#
# The unit file cannot name an account that differs by host, so the choice is
# written to a systemd drop-in. No existing account is modified: the extra
# groups go in the drop-in too.
#
set -euo pipefail

ZM_CONF_DIR="${ZM_CONF_DIR:-/etc/zm}"
DROPIN_DIR="${ZM_API_DROPIN_DIR:-/etc/systemd/system/zm-api.service.d}"
DROPIN="${DROPIN_DIR}/10-account.conf"
FALLBACK_USER="zoneminder"
FALLBACK_HOME="/var/lib/zoneminder"
STATE_DIR="/var/lib/zm-api"
KEY_DIR="${STATE_DIR}/keys"
LOG_DIR="/var/log/zm-api"
CONFIG_DIR="${ZM_API_CONFIG_DIR:-/etc/zm-api}"

# Last assignment of KEY across zm.conf and conf.d/*.conf, the order
# ZoneMinder itself reads them in.
zm_conf() {
  local key="$1" file
  local -a files=()
  [[ -f "${ZM_CONF_DIR}/zm.conf" ]] && files+=("${ZM_CONF_DIR}/zm.conf")
  for file in "${ZM_CONF_DIR}"/conf.d/*.conf; do
    [[ -f "$file" ]] && files+=("$file")
  done
  [[ ${#files[@]} -gt 0 ]] || return 0
  sed -n "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*//p" "${files[@]}" \
    | tail -n1 | tr -d '"'"'" | sed 's/[[:space:]]*$//'
}

user_exists()  { id "$1" >/dev/null 2>&1; }
group_exists() { getent group "$1" >/dev/null 2>&1; }

# The account a previous run settled on.
previous_user() {
  [[ -f "$DROPIN" ]] && sed -n 's/^User=//p' "$DROPIN" | tail -n1 || true
}

# ---------------------------------------------------------------------------
# 1. Pick the account.
# ---------------------------------------------------------------------------
previous="$(previous_user)"
# An install from before the drop-in existed ran as whoever owns the state.
if [[ -z "$previous" && -d "$STATE_DIR" ]]; then
  previous="$(stat -c %U "$STATE_DIR" 2>/dev/null || true)"
fi

web_user="$(zm_conf ZM_WEB_USER)"
web_group="$(zm_conf ZM_WEB_GROUP)"

if [[ -n "$web_user" ]] && user_exists "$web_user"; then
  svc_user="$web_user"
  if [[ -n "$web_group" ]] && group_exists "$web_group"; then
    svc_group="$web_group"
  else
    svc_group="$(id -gn "$web_user")"
  fi
  source="ZM_WEB_USER in ${ZM_CONF_DIR}"
elif [[ -n "$previous" && "$previous" != "root" && "$previous" != "UNKNOWN" ]] \
    && user_exists "$previous"; then
  # No ZoneMinder to follow: keep what this install already runs as.
  svc_user="$previous"
  svc_group="$(id -gn "$previous")"
  source="kept from the previous run"
else
  svc_user="$FALLBACK_USER"
  svc_group="$FALLBACK_USER"
  source="dedicated account, no ZoneMinder found"
  group_exists "$svc_group" || groupadd --system "$svc_group"
  if ! user_exists "$svc_user"; then
    # nologin lives in /usr/sbin or /sbin depending on the distribution.
    nologin="$(command -v nologin || echo /usr/sbin/nologin)"
    useradd --system --gid "$svc_group" --home-dir "$FALLBACK_HOME" \
      --shell "$nologin" --comment "ZoneMinder" "$svc_user"
  fi
fi

# ---------------------------------------------------------------------------
# 2. Extra groups, named only when they exist: systemd refuses to start a unit
#    that lists a group the host does not have (status=216/GROUP).
#      video     local capture devices
#      ssl-cert  TLS keys under /etc/ssl/private (Debian family)
#    plus ZoneMinder's stream-socket and web groups when they are not already
#    the primary group, so live streaming and the zmdc.sock chgrp both work.
# ---------------------------------------------------------------------------
groups=()
for g in video ssl-cert "$(zm_conf ZM_STREAM_SOCKET_GROUP)" "$web_group"; do
  [[ -n "$g" && "$g" != "$svc_group" ]] || continue
  group_exists "$g" || continue
  [[ " ${groups[*]-} " == *" $g "* ]] || groups+=("$g")
done

# ---------------------------------------------------------------------------
# 3. Write the drop-in. The socket directory normally belongs to ZoneMinder's
#    package; it is created here only on a host that has none.
# ---------------------------------------------------------------------------
socks_dir="$(zm_conf ZM_PATH_SOCKS)"
socks_dir="${socks_dir:-/run/zm}"

install -d -m 0755 "$DROPIN_DIR"
tmp="$(mktemp "${DROPIN}.XXXXXX")"
cat >"$tmp" <<EOF
# Written by /usr/share/zm-api/setup-instance.sh on every install and upgrade.
# Edits here are overwritten. To change the account, add a drop-in that sorts
# after this one (systemctl edit zm-api) and restart the service.
#
# Account source: ${source}
[Service]
User=${svc_user}
Group=${svc_group}
SupplementaryGroups=${groups[*]-}
ExecStartPre=
ExecStartPre=+/bin/sh -c 'test -d ${socks_dir} || install -d -m 0755 -o ${svc_user} -g ${svc_group} ${socks_dir}'
EOF
chmod 0644 "$tmp"
mv -f "$tmp" "$DROPIN"

# ---------------------------------------------------------------------------
# 4. State and log directories. systemd re-owns both on start (StateDirectory,
#    LogsDirectory); doing it here too covers a start outside systemd.
# ---------------------------------------------------------------------------
install -d -o "$svc_user" -g "$svc_group" -m 0750 "$STATE_DIR" "$KEY_DIR" "$LOG_DIR"
chown -R "$svc_user:$svc_group" "$STATE_DIR" "$LOG_DIR"

# ---------------------------------------------------------------------------
# 4b. prod.toml and zm-api.env are where an operator puts the database
#     password, the TURN password and API keys, and the packages install them
#     0644. Close them to everyone but root and the service account. base.toml
#     is the packaged defaults and stays readable.
# ---------------------------------------------------------------------------
for f in prod.toml zm-api.env; do
  if [[ -f "${CONFIG_DIR}/${f}" && ! -L "${CONFIG_DIR}/${f}" ]]; then
    chown "root:${svc_group}" "${CONFIG_DIR}/${f}"
    chmod 0640 "${CONFIG_DIR}/${f}"
  fi
done

# ---------------------------------------------------------------------------
# 5. JWT signing keys, generated once and never overwritten.
# ---------------------------------------------------------------------------
if command -v openssl >/dev/null 2>&1; then
  for name in access refresh; do
    priv="${KEY_DIR}/private_${name}_rsa_key.pem"
    pub="${KEY_DIR}/public_${name}_rsa_key.pem"
    if [[ ! -f "$priv" || ! -f "$pub" ]]; then
      (umask 077 && openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
        -out "$priv" 2>/dev/null)
      openssl rsa -in "$priv" -pubout -out "$pub" 2>/dev/null
      chown "$svc_user:$svc_group" "$priv" "$pub"
      chmod 600 "$priv"
      chmod 644 "$pub"
    fi
  done
else
  echo "zm-api: openssl not found. Generate JWT keys in ${KEY_DIR} before starting" >&2
fi

echo "zm-api: service account ${svc_user}:${svc_group} (${source})"

# ---------------------------------------------------------------------------
# 6. An account change leaves behind whatever the old account created. In
#    takeover mode that includes recordings, which the new account must own
#    to delete. Too large and too much not ours to chown from a package
#    script, so say what to run.
# ---------------------------------------------------------------------------
if [[ -n "$previous" && "$previous" != "$svc_user" && "$previous" != "root" \
      && "$previous" != "UNKNOWN" ]]; then
  events_dir="$(zm_conf ZM_DIR_EVENTS)"
  cat >&2 <<EOF
zm-api: the service account changed from ${previous} to ${svc_user}.
zm-api: files the daemons created as ${previous} while zm-api supervised them
zm-api: keep that owner. If this host has run in takeover mode, hand them over:
zm-api:   chown -R --from=${previous} ${svc_user}:${svc_group} ${events_dir:-<events directory>}
EOF
fi
