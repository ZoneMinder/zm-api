# shellcheck shell=bash
# Shared helpers for the packaging e2e stages. Sourced, not executed.
#
# A stage runs every check and reports all the failures at the end, so one run
# shows everything that is wrong instead of stopping at the first.

set -euo pipefail

: "${E2E_DISTRO:?}" "${E2E_FAMILY:?}" "${E2E_ZM:?}" "${E2E_WEB_USER:?}" "${E2E_WEB_GROUP:?}"

API="http://127.0.0.1:8080"
ADMIN_USER="admin"
ADMIN_PASS="e2e-Passw0rd"
# bcrypt of ADMIN_PASS. ZoneMinder ships admin/admin, which zm-api's login
# rejects as too short, and no tool present on every distro makes a bcrypt
# hash, so the hash is fixed here.
ADMIN_HASH='$2y$10$kg84EmgkSvJcXvx7QIiK6uBADL59AZgjGa7ScGhBttdDJWVBRbyIa'
MONITOR_NAME="e2e-cam"
STATE=/var/lib/e2e
ARTIFACTS=/artifacts
mkdir -p "$STATE"

FAILS=0
_out="$(mktemp)"

section() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok()      { printf '  \033[32mok\033[0m    %s\n' "$*"; }
bad()     { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAILS=$((FAILS + 1)); }
note()    { printf '  note  %s\n' "$*"; }

# check "description" command...   passes when the command succeeds.
check() {
  local desc="$1"; shift
  if "$@" >"$_out" 2>&1; then
    ok "$desc"
  else
    bad "$desc"
    sed 's/^/          /' "$_out" | head -25
  fi
}

# check_eq "description" expected actual
check_eq() {
  if [[ "$2" == "$3" ]]; then
    ok "$1 ($3)"
  else
    bad "$1: expected '$2', got '$3'"
  fi
}

# wait_until seconds command...   polls until the command succeeds.
wait_until() {
  local deadline=$((SECONDS + $1)); shift
  until "$@" >/dev/null 2>&1; do
    [[ $SECONDS -lt $deadline ]] || return 1
    sleep 2
  done
}

finish() {
  rm -f "$_out"
  if [[ $FAILS -gt 0 ]]; then
    printf '\n\033[31m%d check(s) failed\033[0m\n' "$FAILS"
    exit 1
  fi
  printf '\n\033[32mall checks passed\033[0m\n'
}

has_zoneminder() { [[ "$E2E_ZM" != "none" ]]; }

# ---------------------------------------------------------------------------
# Database
# ---------------------------------------------------------------------------
db() {
  local client=mariadb
  command -v mariadb >/dev/null 2>&1 || client=mysql
  "$client" -uroot zm -N -B -e "$1"
}

# ---------------------------------------------------------------------------
# ZoneMinder's paths, from the same zm.conf zm-api reads.
# ---------------------------------------------------------------------------
zm_conf() {
  cat /etc/zm/zm.conf /etc/zm/conf.d/*.conf 2>/dev/null \
    | sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//p" | tail -n1
}
socks_dir()  { local d; d="$(zm_conf ZM_PATH_SOCKS)"; echo "${d:-/run/zm}"; }
events_dir() { zm_conf ZM_DIR_EVENTS; }

# ---------------------------------------------------------------------------
# Services and processes
# ---------------------------------------------------------------------------
svc_prop()    { systemctl show -p "$2" --value "$1"; }
svc_active()  { systemctl is-active --quiet "$1"; }
zm_api_pid()  { svc_prop zm-api.service MainPID; }
monitor_id()  { db "SELECT Id FROM Monitors WHERE Name='${MONITOR_NAME}'"; }

# ZoneMinder's own supervisor. The bracket keeps pgrep from matching a shell
# whose command line contains the pattern.
zmdc_pid()    { pgrep -o -f '[z]mdc\.pl'; }
no_zmdc()     { ! zmdc_pid >/dev/null; }

# zm-api exits on SIGTERM by itself. If systemd had to kill it, every restart
# is a TimeoutStopSec-long outage, and in takeover mode a capture gap.
stopped_cleanly() {
  ! journalctl -u zm-api.service --no-pager | grep -E "timed out\. Killing|status=9/KILL"
}

# Pid of the zmc for our monitor, and facts about it.
zmc_pid()     { pgrep -x -o zmc; }
zmc_parent()  { ps -o ppid= -p "$(zmc_pid)" | tr -d ' '; }
zmc_user()    { ps -o user= -p "$(zmc_pid)" | tr -d ' '; }
no_zmc()      { ! zmc_pid >/dev/null; }

# Passive mode: monitor state changes go to ZoneMinder's zmdc.sock (#155).
monitor_state() {
  api PATCH "/api/v3/monitors/$(monitor_id)/state" -H 'content-type: application/json' \
    -d "{\"state\":\"$1\"}" >/dev/null
}
# The live alarm state and Disable/Enable Alarms (zmu -n / -c) go through
# zmc's shared memory (#139).
alarm_action() {
  api PATCH "/api/v3/monitors/$(monitor_id)/alarm" -H 'content-type: application/json' \
    -d "{\"action\":\"$1\"}" >/dev/null
}
alarms_disabled_is() {
  api_json GET "/api/v3/monitor-status/$(monitor_id)" ".alarms_disabled == $1"
}
alarms_toggle_through_shm() {
  alarm_action disable_alarms && wait_until 10 alarms_disabled_is true \
    && alarm_action enable_alarms && wait_until 10 alarms_disabled_is false
}
monitor_stop_stops_zmc()   { monitor_state stop && wait_until 30 no_zmc; }
monitor_start_starts_zmc() { monitor_state start && wait_until 30 zmc_pid; }

# Capture is live when zmc has refreshed Monitor_Status within the last
# few seconds and reports frames arriving.
capturing() {
  [[ "$(db "SELECT COUNT(*) FROM Monitor_Status
            WHERE MonitorId=$(monitor_id) AND CaptureFPS > 1
              AND UpdatedOn > NOW() - INTERVAL 15 SECOND")" == "1" ]]
}

max_event_id() { db "SELECT COALESCE(MAX(Id),0) FROM Events"; }

# A recording newer than event id $1 exists in the database and on disk.
recorded_after() {
  local id path
  id="$(db "SELECT Id FROM Events WHERE Id > $1 ORDER BY Id LIMIT 1")"
  [[ -n "$id" ]] || return 1
  path="$(find "$(events_dir)" -type d -name "$id" | head -1)"
  [[ -n "$path" ]] && [[ -n "$(find "$path" -name '*.mp4' -size +1k | head -1)" ]]
}

# Every file under the events tree is owned by ZoneMinder's account. One
# written by anyone else is a recording ZoneMinder cannot delete after revert.
events_owned_by_web_user() {
  local strays
  strays="$(find "$(events_dir)" ! -user "$E2E_WEB_USER" | head -5)"
  [[ -z "$strays" ]] || { echo "not owned by ${E2E_WEB_USER}:"; echo "$strays"; return 1; }
}

inode() { stat -c %i "$1"; }

# ---------------------------------------------------------------------------
# API
# ---------------------------------------------------------------------------
api_up() { curl -fsS -m 5 "$API/api/v3/server/health_check" >/dev/null; }

login() {
  curl -sS -m 10 -X POST "$API/api/v3/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"${ADMIN_USER}\",\"password\":\"${ADMIN_PASS}\"}" \
    | jq -er .access_token
}

# api METHOD PATH [curl args...]   prints the body, fails on a non-2xx status.
api() {
  local method="$1" path="$2"; shift 2
  curl -fsS -m 20 -X "$method" -H "authorization: Bearer ${TOKEN:?login first}" \
    "$@" "$API$path"
}

# api_json METHOD PATH JQ_FILTER   passes when the filter yields a truthy value.
api_json() { api "$1" "$2" | jq -e "$3" >/dev/null; }

# api_grep METHOD PATH PATTERN   passes when the body contains the pattern.
api_grep() { api "$1" "$2" | grep -q "$3"; }

# api_status METHOD PATH   prints only the HTTP status.
api_status() {
  curl -sS -m 20 -o /dev/null -w '%{http_code}' -X "$1" \
    -H "authorization: Bearer ${TOKEN:?login first}" "$API$2"
}

# The snapshot endpoint takes a keyframe off zmc's stream socket and decodes
# it. A JPEG back proves the socket's permissions and its protocol.
snapshot_is_jpeg() {
  local file="$STATE/snapshot.jpg"
  api GET "/api/v3/monitors/$(monitor_id)/snapshot" -o "$file" || return 1
  [[ "$(head -c 2 "$file" | od -An -tx1 | tr -d ' ')" == "ffd8" ]] \
    && [[ "$(stat -c %s "$file")" -gt 1000 ]]
}

# Live HLS: zm-api reads the stream socket and segments what arrives.
hls_has_segments() {
  local id; id="$(monitor_id)"
  api POST "/api/v3/live/${id}/start" -H 'content-type: application/json' \
    -d '{"enable_hls":true}' >/dev/null || return 1
  local rc=1
  if wait_until 30 api_grep GET "/api/v3/live/${id}/hls/live.m3u8" '\.m4s'; then
    rc=0
  fi
  api DELETE "/api/v3/live/${id}/stop" >/dev/null 2>&1 || true
  return $rc
}

# A player that can't send a header (FFmpeg, so Home Assistant) plays live
# HLS through a signed link: the playlist signs every URI it lists (#153).
hls_plays_from_a_signed_link() {
  local id link url rc=1
  id="$(monitor_id)"
  api POST "/api/v3/live/${id}/start" -H 'content-type: application/json' \
    -d '{"enable_hls":true}' >/dev/null || return 1
  wait_until 30 api_grep GET "/api/v3/live/${id}/hls/live.m3u8" '\.m4s' || return 1
  link="$(api POST "/api/v3/live/${id}/hls/link" -H 'content-type: application/json' \
    -d '{"ttl_seconds":600}' | jq -r .playlist_url)" || return 1
  url="${API}${link}"
  # No access token may appear in what the player is given.
  if curl -sS -m 20 "$url" | grep -q 'token='; then
    note "signed playlist lists an access token"
  else
    local packets
    packets="$(timeout 40 ffprobe -v error -rw_timeout 15000000 -read_intervals '%+4' \
      -select_streams v:0 -count_packets -show_entries stream=nb_read_packets \
      -of csv=p=0 "$url" 2>>"$ARTIFACTS/ffprobe-signed-hls.log" | head -1)"
    note "ffprobe read ${packets:-no} video packets through the signed link"
    [[ "${packets:-0}" -gt 0 ]] && rc=0
  fi
  api DELETE "/api/v3/live/${id}/stop" >/dev/null 2>&1 || true
  return $rc
}

# Live WebRTC: a headless Chromium answers zm-api's offer over the signaling
# WebSocket and must decode frames. Chromium's own WebRTC stack is the peer,
# so this does not depend on the webrtc crate zm-api is built with. Debian
# hosts only: Ubuntu's chromium package is a snap stub.
has_chromium() { command -v chromium >/dev/null 2>&1; }
# ZoneMinder's database account works through the socket only, as on a host
# with skip-name-resolve (database.sh).
tcp_login_refused() {
  local client=mariadb
  command -v mariadb >/dev/null 2>&1 || client=mysql
  ! "$client" -h127.0.0.1 --protocol=TCP -u"$(zm_conf ZM_DB_USER)" -p"$(zm_conf ZM_DB_PASS)" \
    -e 'SELECT 1' >/dev/null 2>&1
}
# Point the monitor at another camera path and restart zmc on it.
use_camera() {
  db "UPDATE Monitors SET Path='rtsp://127.0.0.1:8554/$1' WHERE Id=$(monitor_id)"
  monitor_state stop && wait_until 30 no_zmc \
    && monitor_state start && wait_until 30 zmc_pid && wait_until 60 capturing
}
# Live HLS is H.265: the init segment's sample entry is hvc1 or hev1.
hls_is_hevc() {
  local id file="$STATE/init.mp4"; id="$(monitor_id)"
  api POST "/api/v3/live/${id}/start" -H 'content-type: application/json' \
    -d '{"enable_hls":true}' >/dev/null || return 1
  local rc=1
  if wait_until 30 api_grep GET "/api/v3/live/${id}/hls/live.m3u8" '\.m4s' \
    && api GET "/api/v3/live/${id}/hls/init.mp4" -o "$file" \
    && grep -aqE 'hvc1|hev1' "$file"; then
    rc=0
  fi
  api DELETE "/api/v3/live/${id}/stop" >/dev/null 2>&1 || true
  return $rc
}
webrtc_plays() {
  local id log="$ARTIFACTS/webrtc-chromium.log"
  id="$(monitor_id)"
  local url="file:///e2e/webrtc/play.html?api=${API}&mid=${id}&token=${TOKEN}"
  : >"$log"
  timeout 60 chromium --headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --autoplay-policy=no-user-gesture-required --enable-logging=stderr --v=0 \
    --user-data-dir="$STATE/chromium" "$url" >/dev/null 2>>"$log" &
  local pid=$!
  wait_until 45 grep -q 'E2E_WEBRTC_' "$log"
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  note "$(grep -o 'E2E_WEBRTC_[A-Z]* .*' "$log" | head -1 || echo 'chromium printed no result')"
  grep -q 'E2E_WEBRTC_OK' "$log"
}

# zm-api's own tables exist, which means its startup migrations ran.
own_tables_exist() {
  [[ "$(db "SELECT COUNT(*) FROM information_schema.TABLES
            WHERE TABLE_SCHEMA='zm' AND TABLE_NAME IN ('seaql_migrations','monitor_pipeline')")" == "2" ]]
}

# ---------------------------------------------------------------------------
# Package manager
# ---------------------------------------------------------------------------
pkg_file() {
  local f
  for f in /pkg/zm-api_*.deb /pkg/zm-api-[0-9]*.rpm /pkg/zm-api-*.pkg.tar.zst; do
    [[ -f "$f" ]] || continue
    case "$f" in *debuginfo*|*debugsource*|*-debug-*) continue ;; esac
    echo "$f"
    return 0
  done
  echo "no zm-api package in /pkg" >&2
  return 1
}

pkg_install() {
  local file; file="$(pkg_file)"
  case "$E2E_FAMILY" in
    deb)  DEBIAN_FRONTEND=noninteractive apt-get install -y "$@" "$file" ;;
    rpm)
      if command -v zypper >/dev/null 2>&1; then
        zypper --non-interactive --no-gpg-checks install --allow-unsigned-rpm "$@" "$file"
      else
        dnf -y install "$@" "$file"
      fi ;;
    arch) pacman -U --noconfirm "$file" ;;
  esac
}

pkg_reinstall() {
  local file; file="$(pkg_file)"
  case "$E2E_FAMILY" in
    deb)  DEBIAN_FRONTEND=noninteractive apt-get install -y --reinstall "$file" ;;
    rpm)
      if command -v zypper >/dev/null 2>&1; then
        zypper --non-interactive --no-gpg-checks install --force --allow-unsigned-rpm "$file"
      else
        dnf -y reinstall "$file"
      fi ;;
    arch) pacman -U --noconfirm "$file" ;;
  esac
}

pkg_remove() {
  case "$E2E_FAMILY" in
    deb)  DEBIAN_FRONTEND=noninteractive apt-get remove -y zm-api ;;
    rpm)
      if command -v zypper >/dev/null 2>&1; then
        zypper --non-interactive remove zm-api
      else
        dnf -y remove zm-api
      fi ;;
    arch) pacman -R --noconfirm zm-api ;;
  esac
}

sys_install() {
  case "$E2E_FAMILY" in
    deb)  DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$@" ;;
    rpm)
      if command -v zypper >/dev/null 2>&1; then
        zypper --non-interactive install "$@"
      else
        dnf -y install "$@"
      fi ;;
    arch) pacman -S --noconfirm --needed "$@" ;;
  esac
}
