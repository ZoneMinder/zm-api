#!/usr/bin/env bash
# Passive mode: the REST API works against the live database and the live
# stream sockets, and ZoneMinder keeps supervising its own daemons.
. /e2e/lib.sh

section "API"
check "health check" api_up
TOKEN="$(login)" || { bad "login as ${ADMIN_USER}"; finish; }
ok "login as ${ADMIN_USER}"

version="$(curl -fsS -m 10 "$API/api/v3/host/getVersion")"
note "getVersion: ${version}"
if has_zoneminder; then
  check_eq "reports ZoneMinder's schema version" \
    "$(db "SELECT Value FROM Config WHERE Name='ZM_DYN_DB_VERSION'")" \
    "$(echo "$version" | jq -r '.db_version')"
fi
check "startup migrations created zm-api's own tables" own_tables_exist
check_eq "GET /monitors" "200" "$(api_status GET /api/v3/monitors)"
check_eq "GET /events" "200" "$(api_status GET '/api/v3/events?limit=1')"
# Passive mode reads ZoneMinder's daemons through its zmdc.sock (#156); with
# no ZoneMinder there is no zmdc.sock to read.
if has_zoneminder; then
  check_eq "daemon status comes from zmdc.sock" "200" "$(api_status GET /api/v3/daemons)"
else
  check_eq "no zmdc.sock, no daemon status" "503" "$(api_status GET /api/v3/daemons)"
fi

if has_zoneminder; then
  section "Against the running ZoneMinder"
  mid="$(monitor_id)"
  check "the monitor is listed" \
    api_json GET /api/v3/monitors ".items[] | select(.name == \"${MONITOR_NAME}\")"
  check "monitor status shows frames arriving" \
    api_json GET "/api/v3/monitor-status/${mid}" '(.capture_fps | tonumber) > 1'
  check "recorded events are listed" \
    api_json GET '/api/v3/events?limit=1' '.items | length > 0'
  check "snapshot from the stream socket is a JPEG" snapshot_is_jpeg
  check "live HLS from the stream socket has segments" hls_has_segments
  check "live HLS plays in ffprobe from a signed link" hls_plays_from_a_signed_link
  if has_chromium; then
    check "live WebRTC plays in a headless browser" webrtc_plays
  else
    note "no chromium on ${E2E_DISTRO}: WebRTC browser check skipped"
  fi

  section "An H.265 camera"
  check "zmc captures the H.265 camera" use_camera cam265
  check "snapshot from H.265 is a JPEG" snapshot_is_jpeg
  check "live HLS carries H.265" hls_is_hevc
  check "back on the H.264 camera" use_camera cam

  section "ZoneMinder still in charge"
  check "zoneminder.service running" svc_active zoneminder.service
    check_eq "zmc is supervised by zmdc.pl" "$(zmdc_pid)" "$(zmc_parent)"

  section "Daemons through zmdc.sock"
  check "daemon status is read from zmdc.sock" api_json GET /api/v3/system/status \
    '.running and ([.daemons[] | select(.id | startswith("zmc"))] | length > 0)'
  check "live alarm state is read from shared memory" \
    api_json GET "/api/v3/monitor-status/${mid}" '.alarm_state | IN("idle","prealarm","alarm","alert")'
  check "alarms can be disabled and re-enabled" alarms_toggle_through_shm
  check "live audio level is read from shared memory" \
    api_json GET "/api/v3/monitors/${mid}/audio-level" '.level != null'
  check "stopping the monitor through the API stops zmc" monitor_stop_stops_zmc
  check "starting it again starts zmc" monitor_start_starts_zmc
  check_eq "zmc is supervised by zmdc.pl again" "$(zmdc_pid)" "$(zmc_parent)"
  check "capturing again" wait_until 60 capturing

  # #82: a zm-api restart once deleted ZoneMinder's socket directory.
  before="$(inode "$(socks_dir)/zmdc.sock")"
  systemctl restart zm-api.service
  check "API back after a restart" wait_until 60 api_up
  check "zm-api stopped cleanly for the restart" stopped_cleanly
  check_eq "zmdc.sock survives a zm-api restart" "$before" "$(inode "$(socks_dir)/zmdc.sock")"
  check "stream socket survives a zm-api restart" test -S "$(socks_dir)/stream_${mid}.sock"
  check "still capturing" wait_until 30 capturing

  check "takeover pre-flight passes" zm-api-takeover --check
fi

finish
