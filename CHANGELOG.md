# Changelog

All notable changes to zm-api are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versioning is
[SemVer](https://semver.org/) — carrying the `v3` major from ZoneMinder's API
lineage, so a client written against the legacy `/api/v3` surface has a
recognisable path forward.

## [Unreleased]

### Added

- **Live event stream**, `GET /api/v3/events/stream` (Server-Sent Events):
  `event_start`, `event_end` and `event_updated` (notes changed, where object
  detection writes) for every monitor the caller may see, and each monitor's
  `alarm_state` on connect and on change. Reconnect with `Last-Event-ID` or
  `?since=` to replay missed events. One watcher reads alarm state from
  shared memory four times a second and events from the `Events` table, and
  runs only while someone is subscribed. Heartbeat every 15 s; the stream
  ends at token expiry or loss of access.
- **WHEP for live WebRTC** (RFC 9725): `POST /api/v3/live/{monitor_id}/webrtc/whep`
  takes the viewer's SDP offer and answers `201` with the SDP answer
  (zm-api's candidates included) and the session in `Location`;
  `PATCH` it with trickled candidates, `DELETE` it to stop. Clients whose
  browser makes the offer (Home Assistant's camera WebRTC, WHEP players) can
  now play zm-api's WebRTC without go2rtc. `406` when the offer can't receive
  the camera's codec. A session's user is checked again every minute.
- `[streaming.webrtc]` `nat_ips` (advertise a public address, for 1:1 NAT or
  containers) and `udp_port_min`/`udp_port_max` (media ports from a range
  instead of ephemeral ones), for WHEP and the WebSocket path alike.
- **RTSP server for live video** (`[streaming.rtsp]`, off by default). Players
  that take one URL (Home Assistant, VLC, NVRs) get a monitor's live video at
  `rtsp://host:8554/live/{id}?st=…`, from
  `POST /api/v3/live/{monitor_id}/rtsp/link`. The credential is the signed link
  HLS already uses: one monitor, view only, an expiry, revoked with its user,
  and checked again every minute while a stream plays. ZoneMinder's
  `zm_rtsp_server` takes a username and password in the URL instead. H.264 or
  H.265, RTP interleaved on the RTSP connection (TCP; UDP gets 461), RTSPS on
  `tls_port` with `server.tls`'s certificate. Video only for now.
- **Generate a video for an event** (#187), the first kind in the media job
  queue of `docs/FILTERS_PLAN.md`. `POST /api/v3/events/{id}/videos` queues
  ZoneMinder's Generate Video, with a format from `ZM_FFMPEG_FORMATS`, a
  speed (`rate`) or frame rate (`fps`), a `scale` or `size`, an optional
  greyscale `transform`, and `overwrite`. The video is made from the
  recording, or from the capture JPEGs for an event without one, and named
  as `zmvideo.pl` names its files. `GET /api/v3/events/{id}/videos` lists
  the event's video files with the options read back from their names, and
  `GET`/`DELETE .../videos/{name}` downloads or deletes one (not the
  recording). `GET /api/v3/jobs[/{id}]` shows the queue, and `DELETE` cancels
  a queued job or removes a finished one. Videos are made in-process with
  libav, one job at a time, and jobs a restart interrupted run again.

- **Event export** (#30): `POST /api/v3/events/export {ids, include}`
  streams a zip, built as it goes, with a `{event_id}/` folder per event.
  `include` picks from `details` (`event.json`), `frames` (`frames.json`),
  `images`, `video` and `misc`, and defaults to details and video. Up to 500
  ids; each is checked before anything is sent, so an id that doesn't exist
  or isn't visible is a `404`. Media files are stored as they are and the
  JSON is deflated.

- **Push token registrations** (#175): `/api/v3/notifications` lists, reads,
  registers, changes and removes the Notifications rows that
  zmeventnotification-style push uses, as ZoneMinder's `notifications.json`
  does. Any logged-in user manages their own tokens, and a System editor sees
  everyone's. Posting a token that is already registered updates its row
  (`200`), posting another user's token is a `403`, and another user's row
  is a `404`. `last_notified_at` is in UTC.

- **Monitor actions** (#179): `GET/POST /api/v3/monitors/{id}/actions`,
  `GET/PATCH/DELETE /api/v3/monitors/{id}/actions/{action_id}` and
  `GET /api/v3/monitors/{id}/actions/targets` cover ZoneMinder 1.39.30's
  MonitorActions, the lights and sounds one monitor's events trigger on
  another's device. An action type the target's control can't perform, or an
  audio file outside its range, is a `400`. A change restarts the monitor's
  daemons, since zmc reads the list at start. `POST
  .../actions/{action_id}/run` runs an enabled `Manual` action. It needs
  Control view on the monitor and the target, and builds the command from the
  stored row.

- **Config options say where their value comes from** (#183): each
  `/configs` entry carries `requires` (the option's dependency, as
  ZoneMinder stores it), `source` (`db` or `file`) and `source_file`. When
  `zm.conf` or a `conf.d` file sets the option, the response shows the file's
  value, which is the one in effect, and a `PUT` answers `409`, since the
  file would override the change anyway. A private option set in a file
  still returns an empty value.

- **Signed media links for players that can't send a header** (#153, #34).
  `POST /api/v3/live/{monitor_id}/hls/link` and
  `POST /api/v3/events/{id}/stream/link` mint a playlist URL signed for that
  monitor's live stream or that event's recording, read-only, lasting 30 days
  by default (up to 365). Every URI the playlist lists then carries its own
  signature (`?st=`), valid for minutes on a live stream and renewed with each
  playlist refresh, so FFmpeg, VLC, Safari's native player and Home Assistant
  play it, and a segment URL caught in a log is soon useless. Nothing is
  stored: a link is checked against its user's current permissions and dies
  when the user is logged out everywhere, changes password or is disabled.
  See the Media URLs section of the authentication guide.

- **The schema follows ZoneMinder to 1.39.41** (#152). Eight more upstream
  updates are mirrored as migrations, vendored into the raw chain for
  `migrator bridge`, and the entities follow. `FrameResponse` gains
  `audio_level`, the peak audio level since the previous frame row (0-100, the
  scale `Monitors.AudioThreshold` uses; 0 means silence or no audio).
  Upgrading also does what upstream's updates do to data: Remote/rtsp
  monitors become Ffmpeg with the equivalent url, Libvlc monitors become
  Ffmpeg, NVSocket monitors become Ffmpeg with capturing off, `onvif` controls
  move to `ONVIF`, the duplicated Pelco-D/Pelco-P controls are merged, and a
  Reolink HTTP control is added.

- **Fresh databases start at ZoneMinder 1.39.1 and are upgraded by one portable
  migration per upstream `zm_update-1.39.x`** (docs/DB_VERSIONING_PLAN.md).
  `migrator up` on an empty MySQL/MariaDB *or* Postgres produces the 1.39.1
  create script, then every upgrade upstream has shipped since (currently
  through 1.39.41), the way `zmupdate.pl` does for MySQL alone. Stored
  procedures in the upstream updates (zone-coordinate conversions) are Rust
  loops; MySQL-only trigger rewrites are skipped on Postgres and logged. The
  `schema-parity` CI job now proves the mirrored migrations reproduce upstream's
  create script exactly; a new `postgres-schema` job proves the Postgres
  schema has every table and column MySQL has. Existing MySQL installs are
  unchanged: `migrator bridge` walks the raw chain to the latest vendored
  version and records the migrations that version embodies (#48).

- `Monitors.DeviceClass`, `AudioDetection`, `AudioThreshold` and
  `AudioAlarmScore` (upstream 1.39.30/31) on the monitor create/update
  requests and response; `Controls` gains its light and audio capability
  columns (#48).

- `Reports.CreatedBy` is read and written (#29). The column has existed since
  1.37 but was never modelled, so it was neither stored nor returned.
  Attribution comes from the authenticated token rather than the request body —
  letting a client name the creator is forging authorship. (`description`,
  the other half of that issue, is not possible: there is no such column.)

- **Native replacements for three Perl maintenance daemons** — `zmstats.pl`,
  `zmaudit.pl` (database side) and `zmtelemetry.pl` — each independently
  switchable under `[maintenance]` and all off by default, so an existing
  install keeps running the Perl until the operator moves over. Only one of a
  pair may run: in takeover mode an enabled native job stops the supervisor
  starting its Perl counterpart; in passive mode disable the Perl daemon in
  ZoneMinder yourself.
  <br>**Stats** samples CPU and memory into `Server_Stats`, evicts stale
  `Monitor_Status` heartbeats, ages events out of the `Events_Hour/Day/Week/Month`
  windows and resyncs the counters they feed, and prunes `Logs` and `Sessions`
  under ZoneMinder's own retention settings.
  <br>**Audit** removes `Frames`/`Stats` rows whose event is gone, deletes events
  that never recorded a frame, closes events left open by a capture daemon that
  died, and recomputes `Event_Summaries` and `Storage.DiskSpace` from the rows
  they summarise.
  <br>**Telemetry** posts the same anonymous report on the same schedule.
  <br>Three deliberate differences from the Perl, each because the original is
  wrong rather than because this is simpler: archived events are genuinely
  skipped when deleting frameless events (zmaudit tests a column it never
  selects, so its guard never fires); `dry_run` writes nothing at all (zmaudit's
  `--report` still performs updates, log pruning and counter resyncs); and
  telemetry performs **no geolocation lookup** — zmtelemetry calls `ipinfo.io` on
  every collection, disclosing the server's public IP to a third party under the
  heading of anonymous statistics. Those fields are still sent, always
  `"Unknown"`.
  <br>Retention limits and the telemetry interval are read from ZoneMinder's
  `Config` table but **parsed** rather than interpolated: the Perl splices
  `ZM_LOG_DATABASE_LIMIT` straight into SQL and `eval`s `ZM_TELEMETRY_INTERVAL`
  as code, so a `Config` row is an injection surface in both.
  <br>**The filesystem half** reconciles event directories against `Events`
  rows, and is off even when the audit is on. It deliberately does not work the
  way `zmaudit.pl` does: zmaudit computes a directory from `StartDateTime` and
  `rm -rf`s the result, so a timezone difference, corrupted timestamp, changed
  `Scheme` or wrong `StorageId` makes it remove an unrelated directory with no
  error. Here nothing destructive acts on a computed path — directories are
  found by walking, identified from evidence inside them, and only an enumerated
  path is ever moved. A directory nothing identifies is reported and left alone.
  <br>Orphans are moved to a quarantine directory rather than deleted (an atomic
  rename within the storage, swept after `quarantine_retention_days`); a pass
  refuses if the storage is missing or has no monitor directories, so an
  unmounted volume cannot orphan the whole database; an orphan must be seen in
  two consecutive passes before anything happens; and a derivation canary
  compares computed against actual paths for events found by both routes,
  disabling the filesystem half on sustained disagreement. That last one turns
  the timezone class of bug from silent data loss into an alarm.

- **zm-api can serve the zm-web browser UI itself** (`[web] enabled = true`,
  `APP_WEB__ENABLED`). One process instead of a reverse proxy in front of two:
  the UI and the API share an origin by construction, so CORS stops applying,
  and TLS is already handled by `[server.tls]` / `[server.acme]`. Includes the
  SPA fallback so a refresh on `/events/123` works, `immutable` caching for
  hashed assets with `no-cache` on `index.html`, and a configurable
  Content-Security-Policy applied to UI responses only.
  <br>API paths keep their JSON 404 envelope — the SPA fallback never shadows
  `/api/`, `/swagger-ui`, `/api-docs` or `/.well-known/`, so a mistyped endpoint
  still fails loudly instead of returning an HTML page with status 200.
  <br>Off by default; enabling it with no `index.html` present logs a warning
  and serves the API anyway rather than refusing to start.
- **Still images are rotated to match the monitor's `Orientation`.**
  `/events/{id}/thumbnail` and `/monitors/{id}/snapshot` now return an upright
  JPEG, as ZoneMinder's own image view does — previously a `ROTATE_90` camera
  produced sideways thumbnails, and the failure was silent because nothing
  errored. Stills only: rotating live video would mean re-encoding, and a client
  can do it in CSS for free. `ROTATE_0` keeps the existing zero-copy path.
- Man pages: `zm-api(8)`, `zm-api.env(5)`, `zm-api-takeover(8)`, `zm-api-db(8)`.
- `zm-api --help`, `--version`, and `--openapi` (writes the OpenAPI spec to
  stdout, so the API surface can be diffed or fed to a client generator without
  running a server). All three work before configuration is loaded, so they
  still answer on a host whose config is broken.
- The migration tool ships as `/usr/bin/zm-api-db` in all three packages. It was
  previously built but packaged nowhere, leaving no upgrade path for an existing
  ZoneMinder database.
- `server.allowed_origins` (`APP_SERVER__ALLOWED_ORIGINS`) as a documented,
  `APP_`-prefixed setting, accepting a TOML array or a comma-separated string.
- A documentation site (mdBook) published to GitHub Pages, covering install,
  configuration, deployment architecture, TLS, passive/takeover mode, and
  permissions — plus a browsable API reference rendered from the OpenAPI spec,
  which CI exports from the freshly built binary so it cannot drift from the
  code. `docs/` is now the contributor-facing plan tree only.
- The OpenAPI spec is exported in CI and attached to each release, alongside a
  `SHA256SUMS` file covering every artifact.
- Release notes are taken from this file's entry for the tag, falling back to
  GitHub's generated commit list only when there isn't one.
- `scripts/check-version-consistency.sh`, run in CI before any package is built,
  so a half-finished version bump fails fast.
- `openapi.json` is committed as a reviewed baseline, and CI fails a pull
  request whose spec change could break a deployed client — a removed endpoint,
  a replaced response shape, a response field no longer guaranteed, a dropped
  enum value, or a request that gained a required field. New endpoints and new
  optional fields pass with a notice. This is the guard that would have caught
  the `/me` change above in the pull request that made it.

- `GET /api/v3/stats` takes `event_id`, `frame_id` and `zone_id` (#141), so a
  client can fetch one frame's or one event's rows without paging the table.
  `frame_id` alone is a 400, because frame numbers restart in every event.

- The log page's remaining filters (#142). `GET /api/v3/logs` takes `level` as
  a comma list of exact levels (`-1,-5` for warnings and audit rows), `ids`,
  and `sort_by` on any column. `unfiltered_total=true` adds `total_unfiltered`,
  the whole table's row count; it is opt-in because it costs a second
  `COUNT(*)` on every poll. `GET /api/v3/logs/components` lists the components
  in the table. `DELETE /api/v3/logs` takes the same `ids`, and an empty or
  unparseable `ids` is a 400 rather than "clear everything".

- `GET /api/v3/events` (#136): `tag_id=none` keeps untagged events and
  `tag_id=any` keeps events with at least one tag (legacy's `0` and `-1` are
  accepted for the same), `tag_op=not_in` inverts the tag filter, `sort` takes
  `archived`, `emailed` and `disk_space`, and `totals=true` adds
  `total_length_seconds` and `total_disk_space` summed over the whole result
  instead of the page. Sorting on tags and a free-text `q` are not included.

- **Install tests that run the package against ZoneMinder master**
  (`packaging/e2e/`, the `Packaging` workflow). The built package is installed
  on a container running systemd, beside a ZoneMinder from upstream's master
  repository that is recording from a synthetic RTSP camera, and walked through
  passive mode, takeover, revert, upgrade and removal, checking at each step
  that capture and recording carried on. Ubuntu 24.04 (amd64 and arm64) and
  Debian 13 run that flow; Fedora, openSUSE and Arch, which have no upstream
  master package, run the install, account, upgrade and removal checks. It
  runs on packaging pull requests, on `master`, and nightly, and the `Release`
  workflow now holds back any package that fails it.

- `zm-api-takeover --check` runs the pre-flight checks and changes nothing.
  `--force` takes over despite a failed check.

### Fixed

- **`POST /events` works.** It never set `Events.StateId`, which is NOT NULL
  with no default, so every create failed with 500. It now records the
  active run state, or 0, as zmc does.
- **PTZ commands reach zmcontrol.pl's socket where ZoneMinder puts it.** The
  Perl bridge looked for `zmcontrol-{id}.sock` in `/run/zm` whatever
  `ZM_PATH_SOCKS` said. ZoneMinder's RPM packages use `/run/zoneminder`,
  where it missed the socket and ran zmcontrol.pl for every command.
- **Starting a daemon zm-api won't run is 400.** `POST /daemons/{id}/start`
  for a command that isn't one of ZoneMinder's daemons answered 200 with
  `"success": false`.
- **HTTP/2 with Let's Encrypt (ACME).** In ACME mode the TLS config came
  from rustls-acme's default, which advertises no ALPN protocol, so browsers
  spoke HTTP/1.1 and its six connections per host held back HLS grids. It
  now offers `h2` and `http/1.1`, as certificate-file mode already did.

- **`localhost` reaches MariaDB through its socket on openSUSE too.** zm-api
  connects to `localhost` through the standard socket, as libmysqlclient does
  (#217), but its list lacked openSUSE's `/run/mysql/mysql.sock`, so it fell
  back to TCP. On a server with `skip-name-resolve` the `zmuser@localhost`
  grant then refused it and zm-api didn't start.
- **Event video byte ranges follow HTTP.** `GET /events/{id}/video` and
  `/stream/video.mp4` answered a range past the end of the file with 400; it
  is now 416 with `Content-Range: bytes */<size>`, as players expect. A range
  whose last byte is past the end (`bytes=0-99999999`) now returns the rest
  of the file instead of 400 (RFC 9110 §14.1.2).
- **Changing a monitor you can see but not edit is 403, not 404.** A user
  with a View row on a monitor got 404 from `PATCH /monitors/{id}` (and the
  other monitor writes) although `GET` showed it. A monitor outside the
  caller's scope is still 404.
- **A duplicate username is 409.** `POST /users` with a username already in
  use returned 500 `DATABASE_ERROR`; it now returns 409 Conflict.

- **The stream socket reader identifies codecs by name.** zmc's HELLO now
  carries FFmpeg's codec name (`hevc`, `aac`, tag `0x0B`, ZoneMinder#5336).
  zm-api uses it ahead of the numeric `AVCodecID`, which changes between
  FFmpeg versions, so H.265 and audio work when zmc and zm-api link different
  FFmpegs. A zmc that sends no name falls back to the id, then to the
  parameter sets (#212).

- **A new monitor gets a full-frame zone.** ZoneMinder's monitor editor adds
  an "All" zone to every monitor it creates, and `POST /api/v3/monitors` did
  not, so a Modect monitor added through the API never alarmed. It now adds
  "All" in the same transaction. It covers the whole picture with the
  thresholds of ZoneMinder's "Default" zone preset, which are percentages of
  the zone, so they fit any resolution or orientation. (ZoneMinder's PHP
  editor still writes pixel counts into these columns, which zmc has read as
  percentages since 1.39.3, so its new zones can't alarm. That needs fixing
  upstream.)
- **Zones turn with the monitor.** Changing a monitor's `orientation` now
  moves its zones to match the turned or flipped picture, using the same
  clockwise turns as zmc. Before, zones stayed put and covered a different
  part of the scene. Percentage and pixel zones are both handled.
- **A ZoneMinder schema older than this build no longer stops capture**
  (#147). At startup zm-api compares ZoneMinder's schema version with the
  one it mirrors. When the schema is behind, zm-api logs it as an error,
  does not upgrade ZoneMinder's tables by itself, and doesn't take over the
  daemons, so capture is left alone. Until the schema is upgraded, every API
  call answers `503` naming the fix (`zm-api-db up`, or `bridge` for a
  database not on the migration system); the health check and version still
  answer. A schema that ZoneMinder's own packages have already brought
  forward counts as current. `zm-api-db status` starts with "DB behind
  binary" when that applies, and zm-api-db falls back to the database
  `zm.conf` names when given no URL.

- **Event time filters use the server's time zone** (#173). `start_time` /
  `end_time` on `GET /events` (and the times on `POST /events`, saved-search
  date terms and the audit report window) were turned into naive UTC and
  compared against ZoneMinder's local-time columns, so on any server not on
  UTC the window was off by the offset. They now convert to server-local
  time, the inverse of what responses already do.
- **PTZ light and audio commands** (#190), following the iris and power
  routes: `POST /ptz/monitors/{id}/light/{on|off}` and
  `/indicator-light/{on|off}`, `GET /ptz/monitors/{id}/light` (each light
  `on`, `off` or null, from the control module's status reply),
  `POST .../audio/play/{file}` (checked against the control's sound range),
  `/audio/stop` and `/audio/volume/{up|down}`. Each answers 400 when the
  monitor's control can't do it, and `PtzCapabilities` gains `light` and
  `audio` so a client shows only what it can invoke.
- **`GET /api/v3/monitors/{id}/audio-level`**, the live audio level for the
  monitor editor's meter (#180): `level` (0-100) and `alarm` from zmc's
  shared memory, null while zmc isn't running, with the monitor's
  `threshold` and `detection`. Each call asks zmc to keep measuring for ten
  more seconds even with audio detection off, as ZoneMinder's editor does.
- **`POST /api/v3/system/privacy`** records the answer to the privacy
  statement as ZoneMinder's privacy page does (#189): `ZM_SHOW_PRIVACY` off,
  `ZM_TELEMETRY_DATA` following the answer. System edit.
- **Controls carry their light and audio capabilities** (#177):
  `can_light`, `can_indicator_light`, `can_audio_play`, `min_audio_file`,
  `max_audio_file` and `can_audio_volume`, on the response and on create and
  update.
- **Zone presets carry their thresholds** (#188): the fourteen threshold
  columns a preset applies, on the response and on create and update, named
  as on `ZoneResponse`.
- **`component` on `GET` and `DELETE /logs` takes a comma list** (#186), as
  `level` does, for the log page's multi-select and for clearing by it.
- **`PATCH /storage/{id}` takes `do_delete`** (#185), so the switch that stops
  ZoneMinder deleting media from an area can be changed after creation.
- **Login accepts short passwords** (#174). `POST /auth/login` rejected any
  password under 6 characters before checking it, so ZoneMinder users with
  short passwords, the default `admin`/`admin` included, couldn't log in.
  Setting a password still enforces a minimum.
- **`EventResponse` carries `max_score_frame_id`** (#176), the frame a client
  reports as the match (pyzm's `snapshot` frame resolves through it).
- **Live and recorded HLS play in header-less players** (#153). Playlists
  fetched with a URL credential listed bare URIs, so FFmpeg and Home Assistant
  got the playlist and then a 401 on the init segment and every segment. They
  now list signed URIs. A playlist fetched with `?token=<JWT>` lists
  signatures rather than copying the access token into its URIs.
- **Recorded-event playlists play in FFmpeg** (#154). Segment URIs had no
  extension, which FFmpeg's HLS demuxer refuses; they are now
  `segment/N.m4s`, and a bare `N` still works.
- **Start, stop and restart of ZoneMinder fall back to `zmpkg.pl`** (#157)
  when `systemctl` fails, as ZoneMinder's own API does. The fallback was
  `zmcontrol.pl`, the PTZ script.
- **The OpenAPI spec documents `PATCH /monitors/{id}/state` with its own body**
  (#158), `MonitorStateRequest` (`{"state": "start|stop|restart"}`). Two
  structs shared the name `UpdateStateRequest`, and the run-state one won.
- **Takeover no longer runs `zma` per monitor** (#160). Analysis has run
  inside `zmc` since 1.36, and on 1.39 `zma -e <event>` re-analyses one event
  and exits, so the supervisor restarted it forever.
- **Takeover knows `zm_rtsp_server`** (#161), and starts it when
  `ZM_MIN_RTSP_PORT` is set, as `zmpkg.pl` does. `zmdc.pl start
  zm_rtsp_server` failed as an invalid daemon spec.
- **PTZ capabilities no longer advertise gain, white balance or auto-scan**
  (#146). No command exists for them (and no ZoneMinder control module
  implements them), so clients drew controls that did nothing. They read as
  unsupported until a command exists.
- **`migrator bridge` leaves triggers alone on a database past its cutover**
  (#162), where they are upstream's newer ones.
- **The synopsis pipeline produces cutouts and plates** (#128). For a
  synopsis monitor the generated zm-next pipeline put `review_export` straight
  under capture, where it got compressed packets and produced nothing, passed
  it two keys its schema rejects, and placed nothing to make background
  plates. It now follows zm-next's synopsis layout: `decode_ffmpeg(rgb24)` →
  `review_export`, and `decode_ffmpeg(gray)` → `motion_pixel_diff` with
  `plate_export`, with the frame size taken from the monitor and the plate
  directory set when the pipeline is composed (stored graphs included).
- **Monitor changes reach the capture daemons in passive mode** (#155).
  Creating, editing and deleting a monitor, and `PATCH /monitors/{id}/state`,
  now start, restart or stop its `zmc` (and `zmcontrol.pl` when controllable)
  through ZoneMinder's `zmdc.sock`, as ZoneMinder's own API does. The state
  change answered 200 and did nothing, edits waited for something else to
  restart `zmc`, and a delete left `zmc` running. An edit in takeover mode
  restarts the monitor's daemons too.
- **The live alarm state, and Disable/Enable Alarms** (#139).
  `GET /api/v3/monitor-status/{id}` adds `alarm_state` (`idle`, `prealarm`,
  `alarm`, `alert`), `alarms_disabled` and `analysing` from the monitor's
  shared memory while zmc runs, and `PATCH /api/v3/monitors/{id}/alarm` takes
  `disable_alarms` and `enable_alarms`, ZoneMinder's `zmu -n` and `zmu -c`.
- **Each storage reports its filesystem** (#145): `StorageResponse` gains
  `filesystem` (`total_bytes`, `used_bytes`, `available_bytes`,
  `usage_percent`) for the disk its path is on, as `df` reports it, or null
  when the path can't be read from this server. It is the disk's own size,
  labelled as such, beside the event bytes; the capacity declared for
  retention (docs/STORAGE_PLAN.md) is separate.
- **Encoder templates and menu items** (#144). `/api/v3/encoder-templates`
  and `/api/v3/menu-items` are CRUD over ZoneMinder 1.39's `EncoderTemplates`
  and `Menu_Items` (System-gated), and `GET /api/v3/system/menu` returns the
  enabled menu items in order for any logged-in user, so a client can render
  the navbar the operator configured instead of a hard-coded one.
- **ZoneMinder 1.39 roles** (#143). `GET/POST /api/v3/roles` and
  `GET/PUT/DELETE /api/v3/roles/{id}` manage `User_Roles`, and users gain
  `role_id`. A user's permissions are now, per feature, the higher of their
  own level and their role's, as ZoneMinder computes them, at login, on
  refresh and for signed media links; and the role's per-monitor and
  per-group grants feed the monitor ACL beneath the user's own. A user
  whose access came from a role was gated on their own levels alone.
- **`GET /api/v3/reports/event-audit`**, legacy's Audit Events Report (#36):
  per monitor, the events overlapping a window, their span, the smallest and
  largest gap between one event's end and the next one's start (negative
  where they overlap), and the events whose video is missing or empty on
  disk. Defaults to the hour ending an hour ago, as legacy does; at most
  10000 events per report.
- **`GET /events/counts/{hours}?monitor_id=`** gives one monitor's hourly
  event counts, for the console's per-monitor sparkline (#36), instead of
  paging up to a thousand events per monitor to bucket them.
- **`GET /events/{id}` carries the event's directory** as `path` (#140),
  resolved from its storage and scheme the way ZoneMinder's event page shows
  it, so a client need not reimplement the Deep/Medium/Shallow layout.
- **`GET /events?q=` searches the way legacy's events search box does**
  (#136): a substring match over id, monitor, storage, name, cause, start and
  end time, length, frame counts, scores, archived, emailed, notes and disk
  space, any of which may match.
- **A monitor can be created with only the fields that matter** (#159).
  `CreateMonitorRequest` required 84 of its fields with no server-side
  defaults; now only `name` and `type` are required, and an omitted field
  takes ZoneMinder's column default (`PreEventCount` 10, `SaveJPEGs` 3, and
  so on), so a monitor created through v3 starts out the way one created in
  ZoneMinder does. Columns the response already returned but no request
  could set are now settable on create and update: `go2rtc_enabled`,
  `wall_clock_timestamps`, `default_player`, `encoder_hw_accel_name`,
  `encoder_hw_accel_device`, `stream_channel`, `what_display`, and on create
  the object-detection fields.
- **Daemon and system status work in passive mode** (#156).
  `/api/v3/daemons*` and `/api/v3/system/status` answered 503; they now read
  what `zmdc.pl` supervises from its `zmdc.sock`, with this host's load,
  memory and disk, and the per-daemon start/stop/restart/reload endpoints
  send their command to it.

- **Alarm control through shared memory works on ZoneMinder 1.39** (#148,
  PR #149 by Nic Boet). `SharedData` grew to 888 bytes on master and zm-api
  assumed 872, so `PATCH /monitors/{id}/alarm` wrote `TriggerData` 16 bytes
  early: no alarm, and the cause text landed on `TriggerData.size`, which made
  zmwatch restart the capture daemon. The offset now comes from the `size`
  field zmc writes, and a layout zm-api does not recognise is refused instead
  of written at a guessed offset.
- **`migrator bridge` no longer stamps an older ZoneMinder version over a
  newer database** (#150, PR #151 by Nic Boet). On a database past the
  mirrored chain it wrote `ZM_DYN_DB_VERSION` and `ZM_DYN_CURR_VERSION` back
  to the cutover version, so ZoneMinder re-ran its newer updates and
  `getVersion` reported the wrong schema. The rows are only ever raised.
- **`GET /monitors` and `GET /monitors/{id}` work on ZoneMinder 1.39.37 and
  later.** Upstream dropped `Monitors.RTSPDescribe` (the install tests against
  master caught it as a 500: `Unknown column 'Monitors.RTSPDescribe'`). The
  column is gone from the entity; `rtsp_describe` stays in the request and
  response bodies, ignored on the way in and always `null` on the way out, so
  existing clients keep working. Reported by zm-holodeck's review as well.
- **The Arch package depends on `ffmpeg`.** The binary links the FFmpeg
  libraries, and pacman has no automatic shared-library dependencies, so on a
  host without FFmpeg the service failed to start with "libavutil.so.61:
  cannot open shared object file".
- **The RPM builds on EL9.** The debugsource subpackage was always empty for a
  Rust binary, which rpm on Rocky 9 treats as a build error; it is no longer
  generated.
- **The package now runs zm-api as ZoneMinder's own account, and the previous
  fixed `zoneminder` account is gone from the unit.** ZoneMinder's daemons run
  as the web server's user (`www-data`, `apache`, `wwwrun`, `http`, by
  distribution), and upstream's packages close its files to everyone else. As
  `zoneminder`, zm-api could not read `/etc/zm/zm.conf` on Debian or Ubuntu
  (mode 0640, owned by `www-data`), so a fresh install beside upstream's
  package crash-looped on "Access denied for user 'username'", and after
  takeover `zmc` could not have written the events tree. The post-install
  script now reads `ZM_WEB_USER` and `ZM_WEB_GROUP` from `zm.conf` and writes
  them to `/etc/systemd/system/zm-api.service.d/10-account.conf`; a host with
  no ZoneMinder still gets a dedicated `zoneminder` account. No existing
  account is modified. **On upgrade the service account changes.** State and
  logs follow automatically; recordings made in takeover mode under the old
  account need the `chown --from=` the upgrade prints.
- **zm-api exits on SIGTERM.** A clean shutdown returned `Ok` into a join that
  only ever returned on an error, so the process sat idle until systemd killed
  it at `TimeoutStopSec`. Every stop and restart took 50 seconds and ended in
  SIGKILL, which in takeover mode was a 50 second capture gap on each upgrade.
- **The unit starts on distributions without an `ssl-cert` group.** The shipped
  `SupplementaryGroups=video ssl-cert` named a Debian-only group, and systemd
  refuses to start a unit that lists a missing one, so the RPM and Arch
  packages could not start. Extra groups are now written per host, and only
  those that exist. `WorkingDirectory` no longer fails the start on a host
  without ZoneMinder's share directory.
- **A ZoneMinder schema newer than the mirrored chain no longer blocks zm-api's
  own tables.** First start against upstream master (1.39.35 against the
  mirrored 1.39.33) failed its migration step with "vendor the newer upstream
  updates first" and left `monitor_pipeline` and friends uncreated. Such a
  schema embodies every mirrored migration, so they are stamped and zm-api's
  own migrations run, with a warning that newer upstream columns are not yet
  available through the API.
- **Socket and binary paths follow `zm.conf`.** `daemon.socket_path`,
  `streaming.zoneminder.socks_path` and `daemon.bin_path` left at their
  defaults now take `ZM_PATH_SOCKS` and `ZM_PATH_BIN`. The Red Hat family
  keeps ZoneMinder's sockets in `/var/lib/zoneminder/sock`, not the `/run/zm`
  the package assumed. The shipped `zm-api.env` no longer sets either.
- **`zm-api-takeover` checks before and after the switch.** It refuses to take
  over when zm-api is not up in passive mode, runs as a different account from
  ZoneMinder, or cannot write the events and socket directories. After the
  switch, if zm-api does not stay up, or `zmc` was capturing and has not been
  started again, it undoes the switch, restarts `zoneminder.service` and exits
  3. Previously a failed takeover left the host recording nothing.
- **Removing the package in takeover mode hands control back to ZoneMinder**
  before stopping the service. It used to leave `zoneminder.service` disabled
  with no supervisor running.
- **A package upgrade restarts the service.** The Debian postinst only ran
  `systemctl start`, so the old binary kept running until the next reboot.
- Purging the `.deb`, or removing the `.rpm` or Arch package, removes the
  account drop-in.

- **Over-long request fields are a 400 naming the field, not a 500** (#55).
  About forty request structs had no length rule for columns that are
  `varchar(N)` or `tinytext`, so anything past the width reached MySQL and
  came back as a truncation error. Every bounded field on those structs now
  carries the column's cap, the handlers that never called `validate()` do,
  and the caps count *characters* for `varchar` (a 64-character non-ASCII name
  is legal) and *bytes* for `tinytext` (which is 255 bytes). The monitor
  requests' 26 `#[garde(skip)]` fields are bounded the same way. The 1406→400
  safety net stays for columns nobody has enumerated.
- **The watchdog judges `zmc` by its capture heartbeat, not CPU time** (#123).
  `zmwatch.pl` reads the heartbeat in the monitor's shared memory; the CPU
  heuristic could not see a capture loop that was alive and spinning but no
  longer capturing. The health loop now reads the heartbeat (from
  `ZM_PATH_MAP`) for each running `zmc` past its startup grace, and falls back
  to CPU time only when there is no segment to read — an unreadable segment is
  not treated as hung, so a wrong path cannot restart every camera each tick.
- **The audit no longer closes or deletes a video-only event that zmc is still
  writing** (#124). Frame freshness (#96) is blind to an event that has no
  `Frames` rows at all, so such an event was deleted as empty, or closed as
  unclosed, after `min_age_seconds`. Both sweeps now also require evidence the
  daemon is done with it: a newer event on the same monitor, no live
  `Monitor_Status` row, or an age past the monitor's `SectionLength` plus the
  grace period.
- **The hung-daemon watchdog could never fire** (#73). `check_activity` stamped
  a timestamp on every sample and `appears_hung` then asked whether that stamp
  was older than `watch_max_delay_seconds` — microseconds later it never was.
  A `zmc` blocked on a stalled RTSP read stayed dead until an operator noticed,
  while the docs said `zmwatch.pl` was replaced. The stamp now moves only when
  CPU time advances, so its age is the stall.

- **A second `startup` on a running supervisor SIGKILLed every daemon** (#74).
  The `pkill -9` orphan sweep ran inside `start_all_daemons`, which the legacy
  socket `startup`/`pkg_start` commands and `POST /system/startup` also call.
  Because the entries still read Running nothing respawned them until the
  health tick and first backoff — a 10–20s capture gap on every monitor. The
  sweep now runs once per manager. It also uses `pkill -x`: the unanchored
  default matched `zmaudit.pl` for "zma" and `zmcontrol.pl` for "zmc".

- **A daemon stopped and started again was never crash-supervised** (#78).
  `stop_daemon` clears `auto_restart` so the health loop does not resurrect a
  deliberate stop, but nothing re-armed it on the next spawn (monitor restart,
  reconcile). Supervision is re-armed on every spawn.

- **`zm-api.service` deleted ZoneMinder's `/run/zm` on every stop/restart**
  (#82). It was declared as the unit's `RuntimeDirectory`, so systemd chowned
  it on start and removed it on stop — in passive mode, from under a running
  `zmdc.pl` and every `zmc` stream socket. The unit now only creates the
  directory when it is missing and never touches an existing one.

- **Takeover started the Perl maintenance daemon alongside its native
  replacement** (#87). `base.toml` said to enable the Rust job and disable the
  Perl daemon together, but nothing in zm-api did the second half: the
  singleton gates read only ZoneMinder's `Config`/`Servers` rows. With
  `[maintenance.stats]` on, `zmstats.pl` still ran and both wrote the same
  rows. Each enabled native job now suppresses its Perl counterpart's automatic
  start (an explicit `start` over the socket or REST is still honoured).

- **The daemon manager never knew which server it was** (#75) and the stats
  job read `ZM_SERVER_ID` from an environment variable nothing sets (#100).
  On a multi-server install every host started every monitor's `zmc`, the
  per-server `Servers.zm*` gates were dead, and `Server_Stats` rows landed
  under `ServerId = 0`. The id is now resolved the way ZoneMinder's own daemons
  do it — `ZM_SERVER_ID` from `zm.conf`, else `ZM_SERVER_HOST` against
  `Servers`, else the machine hostname — and shared by both.

- **Three documented `[daemon]` keys were never read** (#88).
  `enable_watchdog` now gates the hung-process check (exit detection and crash
  restarts are never optional), `stats_update_interval_seconds` drives the
  `Servers` status loop instead of a hard-coded 60s, and
  `enable_rest_api = false` leaves the daemon/system routes unregistered.

- **The retention reaper could empty a storage in one pass** (#105). With
  `Events.DiskSpace` NULL — the normal state of a freshly recorded event until
  something backfills it — each deletion credited zero bytes, the free-space
  model never moved, and the loop ran to the end of the table. NULL sizes are
  now measured on disk, free space is re-read from the filesystem every 25
  deletions, and a new `max_deletes_per_pass` (default 500, `0` = unlimited)
  bounds one pass. Alongside it: an unmounted storage (missing or empty
  directory) is skipped rather than reaped against the parent filesystem's
  free space, and the first pass waits 60s after start (#104); a failed media
  removal stops the pass instead of deleting row after row while the disk
  stays full (#106); an event archived after the candidate list was taken is
  left alone (#109); and event deletion now removes the event's `Stats` rows,
  which have no foreign key and were left behind (#110).

- **Audit and stats SQL** — a batch from the same review. The
  `Storage.DiskSpace` resync never ran: its SELECT could not decode the
  unsigned `Id` and the DECIMAL `SUM` into signed Rust integers, and the error
  was swallowed (#90); the columns are cast and a failed job is now part of the
  audit report, so a test asserting a clean pass fails (#101 for the stats
  job likewise). Every "older than N" comparison used a session forced to UTC
  against columns ZoneMinder writes in local time (#98); the connection now
  sets `time_zone = SYSTEM`. The window-counter and total-counter resyncs took
  locking reads inside an UPDATE in the reverse order of ZoneMinder's Events
  triggers (#99); they read with a plain SELECT and write by primary key. The
  orphan-frame sweep full-scanned `Frames` under next-key locks (#94); it
  selects the orphan ids first and deletes by index in batches.
  `max_deletes_per_pass` capped which rows were *looked at* rather than
  deleted (#91). Two `Storage` rows on one path confirmed an orphan in a
  single pass (#93). `min_age_seconds` never applied to directory quarantine
  (#95). `close_unclosed_events` closed live events still receiving frames
  (#96). A Deep-scheme day directory holding one `.{id}` symlink was recorded
  as the event itself (#92); the link is followed to the leaf. A storage whose
  mount dropped, then got a fresh monitor directory from zmc, passed the
  preconditions and had every older row deleted as "no media" (#89); a pass
  now refuses when more rows are missing than events were found. ONVIF
  event-listener rows, which never have frames, were deleted as empty (#97).

- **Supervisor lifecycle** — reconcile only diffed the database against the
  process map in one direction, so a deleted monitor's daemons ran on, and a
  hard-deleted one crash-looped forever (#76). `restart_monitor` slept 500ms
  and spawned over a process still shutting down, orphaning it (#77); starts
  are refused over a `Stopping` entry and restarts wait for the exit. A
  daemon restart latched the process-wide shutdown flag, so the ONVIF event
  listeners exited and never came back (#79); they now watch a separate
  process-exit flag. Background loops that missed the shutdown wake-up
  mid-tick ran on beside their replacements (#80); every startup retires the
  previous generation. Stop/kill on a crashed entry signalled a dead, possibly
  reused pid (#81). Two Local monitors on one device had their shared `zmc -d`
  stopped and restarted every tick (#113). A database still coming up at boot
  left the singleton daemons unstarted for the life of the process (#114); it
  retries. Reconcile respawned a crash-looping daemon every 60s, capping the
  documented backoff (#115). The HTTP drain ran unbounded and before the daemon
  SIGTERM wave, so systemd could SIGKILL zmc mid-stop (#116); both run
  concurrently and bounded, and `TimeoutStopSec` leaves margin. `POST
  /system/state` returned a bare 500 after committing when the restart failed
  (#117).

- **zmdc.sock compatibility** — `status`/`check` ignored the daemon and
  never emitted zmdc.pl's per-daemon lines or words, which the web console
  string-matches (#83). Daemons were keyed by name alone, so `zmdc.pl stop zmc
  -m 1` missed the tracked `zmc -m 1` and `start` spawned a duplicate (#84);
  one canonical `"<command> <args>"` key everywhere. The socket was 0660 in
  the service user's own group, so ZoneMinder's web user could not connect
  (#85); it is chgrp'd to `ZM_WEB_GROUP` (or `daemon.socket_group`) and the
  install script adds the service account to that group. `logrot` sent SIGHUP
  — reload, which drops zmc's camera — to every daemon, so nightly logrotate
  interrupted capture (#86); it sends SIGWINCH like zmdc.pl. The orphan sweep
  missed four Perl daemons and `zm-core`, and never checked for a live
  zmdc.pl before pkilling (#118); the list is derived from the spawn table,
  anchored on the full command line, and takeover refuses while something
  answers on the socket. Command reads had no size or time bound (#119).

- **Smaller ones** — telemetry kept the query string, so `?user=&password=`
  camera credentials were sent (#102); it ignored `ZM_TELEMETRY_DATA` and
  silently minted a fresh uuid every pass on a database without the Config
  row (#103). Deep-scheme deletion removed a timestamp-derived directory with
  no check it held the event, and unlinked the `.{id}` marker from the wrong
  directory (#107); it follows the marker and refuses a directory naming
  another event. `Storage.DoDelete` was never read (#108). The byte quota
  counted only deletable events (#110). The retention guide's grep matched
  nothing and the promised per-deletion reason was never logged (#112). The
  ONVIF listener's reconnect backoff never reset, its renew cancelled an
  in-flight pull whose response the device had already dequeued, and
  listeners were spawned for deleted monitors (#120). `POST
  /monitors/{id}/zmnext` returned 200 with zm-next disabled (#121). Non-DB
  unit tests now cover the reaper's free-space loop, NULL `DiskSpace`, age
  cutoff, quota and cap (#111).

- **The rate limiter made the API unusable for any browser client** (#70). A
  burst of `0` with the limiter enabled was clamped silently to `1`, so one
  request succeeded and everything after it returned 429 — no page in any
  single-page app could load, and nothing said why. A burst that small is never
  intentional, so it is now treated as the misconfiguration it is: the server
  substitutes a usable value and logs an error naming the setting.
  <br>The setting is renamed `rate_limit_period_secs`, because
  `rate_limit_per_second` read as a rate while meaning a *period* — setting it
  to `4` expecting four requests a second gave one request every four seconds.
  The old name is still accepted, so existing configuration keeps working.
  <br>The production defaults are retuned from one token per 25 seconds with a
  burst of 50 to one per second with a burst of 120. The old values let the
  first screen through and then throttled everything after it to one request
  every 25 seconds. Credential brute-forcing is handled separately and far more
  tightly by the auth limiter, which is why the global one can afford to be
  generous.
- **Six duplicate `operationId`s** (#32) — the five AI-model routes collided
  with the camera-model routes, and two unrelated `update_state` handlers with
  each other. A generator silently emits one method and drops the other.
- **Four routes that enforce authentication did not say so in the spec** (#32):
  `/daemons`, `/daemons/{id}`, `/system/status` and the WebRTC signalling
  socket. A generated client reads a missing `security` key as "no token
  needed".
- **`NaiveDateTimeWrapper` claimed `format: date-time`** (#32) while emitting
  `2025-04-24T12:34:56`, which has no offset and is therefore not RFC 3339.
  Generators built an offset-aware parser that rejected every value the API
  sends. It is now described as what it is — local wall-clock time as
  ZoneMinder stores it.
- **Zone `Area` was never computed** (#43). Every zone created through the API
  had `Area = 0` hardcoded, and changing a zone's coordinates never recomputed
  it. That is not cosmetic: when a zone's units are `Percent`, the alarm
  thresholds are stored *relative to* `Area`, so those zones had thresholds that
  silently did not mean what they said. Area is now derived from `Coords` on
  both create and update, matching ZoneMinder's own `getPolyArea` (plain
  shoelace — upstream keeps an inclusive-pixel variant but no longer calls it).
  Coordinates that do not describe a polygon are rejected with a 400 rather than
  stored with a zero.
- **`SaveJPEGs` rejected its own default** (#39). It is a two-bit mask whose
  column default is 3, but the bound was `-1..=1`, so the API refused the value
  ZoneMinder ships with — the same class of bug as #19. The two neighbouring
  fields had the same copy-pasted bound and were also wrong: `VideoWriter` is
  0–2 (disabled / encode / camera passthrough) and `RecordAudio` is 0–1. All
  three confirmed against the upstream monitor form rather than inferred.
- **Storage created through the API could never be reclaimed** (#44).
  `DoDelete` was hardcoded to 0 while the column defaults to 1, so neither the
  retention reaper nor `DELETE /events/{id}` could remove media from a storage
  the API created — the disk fills and nothing says why. Now defaults to 1,
  matching the column, and is settable on create.
- **Over-long values return 400 instead of 500** (#55). Roughly forty request
  fields write to fixed-width columns with no length rule of their own, and each
  turned an over-long value into `DATABASE_ERROR` with no indication of what was
  wrong. A "data too long" error is now mapped to `VALUE_TOO_LONG` / 400 naming
  the offending column. Only the column name is surfaced — the driver's message
  can carry the rejected value and surrounding SQL, and that redaction is
  tested. Per-DTO rules remain better where they exist, since they reject before
  the round trip; this is the net under everything else.
- **Bridged installs kept a legacy collation** (#40), failing upgrade-parity on
  every pull request since #14. The bridge normalised `EncoderTemplates` *to*
  `utf8mb4_unicode_ci`, which is the value the legacy chain creates it with —
  converting toward the old collation guaranteed the mismatch against a fresh
  baseline instead of removing it. It now converges on the database's own
  default, and any other table that drifts is logged by name.

### Security

- **Generate Video can't replace the event's recording, and the queue has a
  limit.** A video's file name comes from the event's name, so renaming an
  event to match its recording (`{id}-video`) and asking for a video with
  `overwrite` replaced the recording with the render. `POST
  /api/v3/events/{id}/videos` now refuses (409) a video whose file would be
  the recording, and a queued job that would land on it fails instead.
  Each user can have 4 videos queued or running (409 after that), and `size`
  can be at most 4 times the event's width and height, the same limit as
  `scale`.

- **RTSP connections must present a working link within 15 seconds.** A
  connection holds one of the RTSP server's slots from the moment it opens,
  and `OPTIONS` needs no link, so a client that kept sending `OPTIONS` could
  hold every slot without ever authenticating and lock out every player. A
  connection that hasn't presented a working link 15 seconds after it opens
  is now closed.

- **PTZ can't be pointed at the server itself.** A monitor's
  `control_address` is set by anyone with Monitors Edit, and PTZ commands
  make zm-api send requests there. It took any address, followed redirects,
  and returned up to 512 bytes of whatever answered in the error, so the
  address could be used to read local services. A control address that
  reaches this server (loopback, its own addresses) or a cloud metadata
  address is now refused with 400, host names included, whether the control
  is native ONVIF or a zmcontrol module. The ONVIF client no longer follows
  redirects or uses proxy settings, and transport and parse errors say only
  that the camera didn't answer, or didn't answer as ONVIF.

- **A role's group grants now limit the groups API, and moving a group needs
  Edit on both parents.** The groups API read only the user's own
  `Groups_Permissions`, so a user whose role was limited to some groups, with
  no rows of their own, could see and edit every group. Monitor access already
  applied the role's group grants to each group's subgroups, so moving a hidden
  group under a group the role grants made its monitors visible. Now the
  role's rows count, with the user's own row deciding first, as for monitors.
  Moving a group with `PUT /api/v3/groups/{id}` needs Edit on the old and the
  new parent, and creating one under a parent needs Edit on that parent.

- **Onboarding a discovered camera needs Monitors Create.** `POST
  /api/v3/discovery/onboard` adds a monitor, but it only needed Monitors Edit
  and unrestricted monitor access, so a user ZoneMinder doesn't allow to add
  monitors could add them this way. It now needs Monitors Create or System
  Edit, as `POST /api/v3/monitors` does.

- **`linked_monitors` can only name monitors you can see.** A user who could
  edit one monitor could link it to any other, including monitors they're
  denied, and so record events off a hidden camera's alarms. `POST` and
  `PATCH /api/v3/monitors` now refuse (400) a link to a monitor the caller
  can't see, with the same message as for one that doesn't exist. The value
  must also be an expression zmc reads: monitor ids, or `id:zone`, joined by
  `|`, `&`, `,` and parentheses. ZoneMinder's zmc stops on a piece that isn't
  a number.

- **Only a monitor's editors can stop its live stream.** `DELETE
  /api/v3/live/{monitor_id}/stop` ends the session for every viewer, but
  Stream View was enough to call it, so any viewer could keep cutting off
  everyone else. It now needs Monitors Edit on that monitor (403 otherwise).
  Viewers don't need it: closing the player is enough, and idle sessions are
  reaped.

- **A huge `hours` no longer crashes the request.** `GET
  /api/v3/events/counts/{hours}` and `/events/counts-by-monitor/{hours}` did
  date arithmetic on the value unchecked, so any signed-in user could send a
  number like `9223372036854775807` and panic the request, which a proxy
  reports as 502. Both now answer 400 unless `hours` is from 1 to 87600 (ten
  years). Zero and negative values, which used to return nothing, get 400 too.

- **Live WebRTC sessions over the WebSocket are rechecked, capped and timed
  out.** A token or signed link opened `/api/v3/live/{id}/webrtc/ws` once
  and the session then ran until the client left. Now:
  - The viewer's access is checked every minute, as WHEP does. A disabled
    or revoked user, or one who lost Stream or the monitor, is cut off.
  - WebSocket and WHEP sessions share one `streaming.webrtc.max_connections`
    cap, taken in a single atomic step. Before, WHEP checked a count, so
    sessions starting together could pass the limit, and the WebSocket had
    no cap at all.
  - A client that doesn't answer the offer within 30 seconds is closed. The
    timer used to start only once it answered.

- **Rate limits can't be dodged with a forged `X-Forwarded-For`.** With
  `trust_proxy_headers` on, which `prod.toml` sets, the limiters took the
  leftmost `X-Forwarded-For` address from any connection. A client reaching
  zm-api directly could send a new address with each request and get a fresh
  login allowance every time. Even behind a proxy the leftmost address is the
  one the client wrote. Now the headers are believed only on connections from
  `server.middleware.trusted_proxies` (new; loopback by default, for Apache on
  the same host). The client is the rightmost address that isn't a trusted
  proxy. A proxy on another host needs adding to `trusted_proxies`.

- **Event playback no longer keeps every played event in memory.** HLS
  playback of a recorded event (`/events/{id}/stream/...`) packaged the
  video into memory and kept it there for the life of the process. Playing
  events one after another grew memory without limit. The cache now holds at
  most 256 MiB, drops the least recently played event first and any event
  not played for 10 minutes, and doesn't keep an event bigger than that. It
  also notices when an event's video file changes (a recording still being
  written, or a regenerated video) and packages it again instead of serving
  the old one.

- **Tags only show events the caller can see.** `GET /api/v3/tags/{id}`
  listed every event with the tag, including ones on monitors the caller is
  denied, and `GET /api/v3/tags` counted them in `event_count`. Both now use
  the caller's visible monitors, as `/events` does. `page=0` on a tag is
  page 1, where it used to overflow.

- **ONVIF inspect can't reach the server itself.** `POST
  /api/v3/discovery/inspect` accepted loopback addresses, so a Monitors Edit
  user could probe the server's own ports and read back what answered. It now
  refuses loopback, the unspecified address, the server's own interface
  addresses and the cloud metadata addresses (169.254.169.254,
  fd00:ec2::254). It no longer follows redirects, and says only that a device
  didn't answer as ONVIF instead of passing on transport or parse errors.
  Cameras on private and link-local addresses work as before.

- **Camera passwords inside monitor URLs are hidden from viewers.** `pass`
  and `onvif_password` were already left out of monitor responses, but a
  password in `path`, `second_path`, `control_address` or `onvif_url`
  (`rtsp://user:pass@host`, `user:pass@host`, or a `pwd=` query parameter)
  went to anyone with Monitors View. `GET /api/v3/monitors` and
  `/monitors/{id}` now show it as `***` unless the caller can edit that
  monitor. Editors still see the URLs as stored, because they edit them.

- **Filters with automatic actions need Edit on every monitor.** `zmfilter.pl`
  runs a saved filter over every monitor's events, whoever owns it. A user
  limited to some monitors could save a filter with AutoDelete, AutoEmail or
  another automatic action, and it would delete or send events from monitors
  they can't see. `POST` and `PUT /api/v3/filters` now refuse such a filter
  (403) unless the caller has Edit on every monitor or System Edit. Restricted
  users can still save filters without actions, and can turn the actions off
  on a filter they own. Filters already in the database are unchanged.

- **`/api/v3/sessions` is removed.** It gave read and write access to
  ZoneMinder's PHP web sessions to anyone with System access. A System View
  user could read another user's session id, which is the classic UI's login
  cookie, and the session data, which holds that user's row with its password
  hash. With the admin's session id they could use the classic UI as admin. A
  System Edit user could create or change session rows, and so forge a web
  login as anyone. zm-api logs in with JWTs and never uses these sessions, so
  the routes (`GET`/`POST /sessions`, `GET`/`PATCH`/`DELETE /sessions/{id}`)
  are gone rather than restricted. The maintenance service still prunes
  expired rows for installs that run the PHP UI.

- **Event videos no longer run the ffmpeg program.** Media jobs ran the
  binary named by `ZM_PATH_FFMPEG` with `ZM_FFMPEG_INPUT_OPTIONS` and
  `ZM_FFMPEG_OUTPUT_OPTIONS` as arguments, so anyone who could write the
  Config table chose the program and its arguments. Videos are now
  decoded, filtered, encoded and muxed in-process with libav.
  `ZM_PATH_FFMPEG` is ignored. The option strings are parsed into an
  allowlist of encoder and muxer settings (`-r`, `-c:v`, `-pix_fmt`,
  `-preset`, `-tune`, `-profile:v`, `-level`, `-crf`, `-qp`, `-b:v`,
  `-maxrate`, `-minrate`, `-bufsize`, `-g`, `-bf`, `-threads`,
  `-movflags`). Anything else is dropped and logged, and
  `PUT /api/v3/configs/{name}` refuses it. A recording is opened only if
  `DefaultVideo` is a plain file name, and only as a local file in a
  recording container, so it can't name a URL, a playlist or another file.

- **Stored values can't change the commands ZoneMinder runs.** Anything that
  becomes part of a command now needs `System: Edit` or is checked first:
  - Filters with `AutoExecute` or a command, and monitor
    `EventStartCommand`/`EventEndCommand`, need `System: Edit`, as in
    ZoneMinder. Other users can't edit a filter that already runs a
    command.
  - Monitor `Path`/`SecondPath` must use a known capture scheme or be a
    local path (go2rtc runs `exec:` sources). `Device` must be under `/dev/`.
    `Storage.Path` is limited to characters a shell treats literally.
    Controls `Protocol` must be a bare Perl module name.
  - zm-api won't start zmc, zma, zmtrack, zmcontrol, zmfilter or zmaudit
    while the rows they read hold such values, including rows written
    outside zm-api. It also refuses zmfilter when the installed version
    doesn't escape filter tags and an `AutoExecute` command uses text tags.
    Unsafe stored values are logged at startup and every hour.

- **Monitor access follows ZoneMinder's rules.** A user's monitors are
  decided as ZoneMinder's `visibleMonitor` and `editableMonitor` decide
  them: their own per-monitor grant, then their groups (a `None` group
  denies), then their role's monitors and groups, then their `Monitors`
  level. A group grant now covers its child groups. A user with no grant
  rows got every monitor before; they now get what their `Monitors` level
  allows, so `Monitors: None` sees none. The grants are read in one query.
- **Adding a monitor needs `Monitors: Create`** (or `System: Edit`), as in
  ZoneMinder. `Edit` changes existing monitors only.
- **Live streaming needs `View` on the monitor**, not `Edit`, for every
  method, since ZoneMinder treats watching as a read.
- **`/live/sessions` and `/live/sources` list only the caller's monitors.**
- **Group membership changes need `Edit` on the group**, and reads list only
  groups the caller can see.
- **Moving a stat to another monitor needs `Edit` on that monitor.**
- **Admin changes to a user's access, and deleting a user, revoke their
  tokens at once.**

- **Private config values are hidden below System Edit** (#184).
  `GET /api/v3/configs` and `GET /api/v3/configs/{name}` returned `Private`
  options in clear to anyone with System View, `ZM_AUTH_HASH_SECRET`
  included, which is enough to sign an admin token (ZoneMinder fixed the same
  in 1.39.41, GHSA-7g3f-93q8-fhwj). Their `value` is now empty unless the
  caller can edit System.
- **Media responses to URL-credentialed requests are no longer cacheable by
  shared caches** (`Cache-Control: private`) and send
  `Referrer-Policy: no-referrer`, so a proxy can't replay a segment fetched
  with a signature and the URL doesn't leak through `Referer`.

- Dependency bumps for RUSTSEC-2026-0338 to 0342 (`quinn-proto` 0.11.19),
  RUSTSEC-2026-0346 to 0349 (`rustls` 0.23.46), and `async-trait` 0.1.92,
  whose generated code tripped clippy's `double_must_use` on current stable.
  The `zip` 3 advisories (RUSTSEC-2026-0367 to 0369) are ignored with a
  justification in `deny.toml`: zip is a build-time dependency of
  `utoipa-swagger-ui` 9 that unpacks the bundled Swagger UI, not runtime code,
  and no release of that crate depends on zip 9 yet. Clearing them needs the
  utoipa 6 migration (#164).

### Removed

- The unused "native WebRTC" engine and session manager
  (`src/streaming/webrtc/`). `AppState` built them at startup, but no
  handler used them; live WebRTC has always gone through
  `src/streaming/live/webrtc.rs`.
- `frame_skip` from the monitor create/update requests and response:
  upstream dropped `Monitors.FrameSkip` in 1.39.24. `motion_frame_skip`
  remains. `User_Preferences.Name` is required on create (NOT NULL and unique
  per user since 1.39.19) (#48).

- **Config blocks nothing implemented** (#53). `[streaming.rtsp_proxy]` declared
  a port and an RTP range that nothing bound, and `[streaming.go2rtc]` a base
  URL that nothing called — an operator could configure either, restart, and get
  no behaviour change and no warning. Both are gone, along with an unused
  request DTO. `Monitors.Go2RTCEnabled` stays: that is ZoneMinder's own column
  and the response passes it through. Removing them is upgrade-safe — no config
  struct denies unknown fields, so a stale block in an existing file is ignored
  rather than refusing to start, and there is a test for that.

- **Enums emitted Rust variant names instead of the values ZoneMinder stores.**
  `#[sea_orm(string_value = …)]` governs only the database mapping, so serde
  fell back to the variant name: `/monitors` reported `Rotate90` where the
  column holds `ROTATE_90`, and `Curl` where it holds `cURL`. Nine enums were
  affected — `Orientation`, `MonitorType`, `DefaultCodec`, `EventCloseMode`,
  `Rtsp2WebType`, `Decoding`, `OutputContainer`, `StorageType`,
  `SynopsisStatus`. It was self-consistent, and therefore invisible: requests
  accepted the same wrong spelling responses emitted, so a client that only ever
  talked to this API round-tripped fine while anything that knows ZoneMinder's
  real values silently mismatched.
  <br>Responses and the OpenAPI schema now carry the DB values. The previous
  spelling is still **accepted** on input via a serde alias, so clients keep
  working while they migrate. A test walks `ActiveEnum::values()` for every
  affected enum, so a newly generated one is covered without anyone remembering.
- **A configured TURN server had no effect.** `AppState` built the WebRTC engine
  from defaults rather than the loaded `[streaming.webrtc]` config, so
  `stun_servers` and `turn` were parsed, validated, and discarded — viewers
  behind symmetric NAT could not connect, with nothing in the log to explain it.
- **CORS was undiscoverable.** The allowed-origin list came from a bare,
  un-prefixed `ALLOWED_ORIGINS` variable that appeared in no config file and no
  documentation, defaulting to localhost. A dashboard on any other origin was
  silently blocked with no string in the repo to grep for. The variable is still
  honoured (with a deprecation warning) so existing deployments keep working; the
  effective list and its source are now logged at startup.
- **`APP_DAEMON__SCRIPT_PATH` shipped a value wrong for Debian and Ubuntu.** One
  env file serves all three package formats, and no single value suits every
  distribution, so daemon paths are now resolved by searching the standard
  locations, with the setting as an override.
- **`Requires=mariadb.service` failed the unit** on hosts using `mysql.service`
  or a remote database. The unit is still ordered `After=` both, and
  `Restart=on-failure` covers a slow database.
- A stream socket the service user cannot open now reports that
  `ZM_STREAM_SOCKET_GROUP` membership is missing, rather than a bare "permission
  denied" naming nothing actionable.
- `docs/tls.md` claimed the systemd unit uses `DynamicUser`; it runs as
  `User=zoneminder`.

### Changed

- **Requests use about a third less CPU.** Measured across 19 read endpoints:
  1.56x less CPU per request and 1.75x lower median latency. Per-request
  INFO logs and `#[instrument]` spans are now DEBUG (the bunyan layer wrote
  a START and END line for every span). Log output to stdout no longer
  blocks the request. A verified access token is cached until it expires, so
  its RSA signature is checked once rather than two or three times per
  request; revocation is still checked on every request. Pooled connections
  are pinged only after 30 s idle, not before every query.

- **Dependencies brought up to current releases.** Every semver-compatible
  update was taken, and these crossed a major line: sea-orm and
  sea-orm-migration 1.1 → 2.0 with sqlx 0.8 → 0.9 (raw statements go through
  the `*_raw` connection methods, `Value::String` is unboxed, `ExprTrait` is
  imported where expressions are built), utoipa 5 → 6 with utoipa-swagger-ui
  9 → 10 (no code changes; `openapi.json` regenerated), quick-xml 0.41 → 0.42
  (string-based events in the ONVIF parsers), rand 0.9 → 0.10, argon2 0.5 →
  0.6 (same PHC hashes), sha1/sha2 0.11, chacha20poly1305 0.11, jsonwebtoken
  11, base64 0.23, tower-http 0.7, garde 0.23, sentry 0.49, jemalloc 0.7,
  itertools 0.15, tokio-tungstenite 0.30 and axum-test 21. The unused mockall
  and rstest dev-dependencies and the unused direct rand_core dependency are
  gone.
- **webrtc 0.17 → 0.21** (#165). 0.20 rebuilt the crate as an async layer
  over a Sans-I/O core (`rtc`), so the live WebRTC session was rewritten
  against it: peer connections come from `PeerConnectionBuilder`, events
  arrive through one `PeerConnectionEventHandler` instead of per-event
  closures, and each track writes samples with its own SSRC and the payload
  type the browser's answer negotiated. Host candidates now come from one UDP
  socket per interface, IPv4 always and IPv6 when the host can open an IPv6
  socket. The 0.12-era workaround that synthesised "connected" from the DTLS
  transport state is gone. The interceptor, webrtc-media, webrtc-util and
  rcgen dependencies are dropped. The signaling protocol is unchanged.
- **The crate declares `rust-version = "1.94"`**, which sea-orm 2 and sqlx 0.9
  need. Debian, Ubuntu, Arch and openSUSE builds already use a toolchain at
  or past it. EL9's rust-toolset is 1.92, so the RPM builder for Rocky now
  installs current stable through rustup, as the Debian builders do; building
  the RPM on EL9 by hand needs the same. The Fedora package and release jobs
  move from Fedora 41, which is end of life and frozen at rust 1.91, to
  Fedora 44.

- **`tag_id` that doesn't parse is a 400** (#136). `GET /api/v3/events` used
  to drop non-numeric values, so `tag_id=abc` quietly became "no tag filter"
  and returned every event.
- **BREAKING: `GET /api/v3/monitors` leaves out soft-deleted monitors** (#137).
  The list had no `Deleted` filter, so a monitor ZoneMinder's web UI had
  deleted (`Deleted = 1`, the row kept until its events are gone) came back
  looking live. Pass `include_deleted=true` to list them; each row's `deleted`
  says which is which. `GET /api/v3/monitors/{id}` still returns a soft-deleted
  monitor, so its page loads while its events exist.
- **BREAKING: frames are addressed by event and frame number.** ZoneMinder
  1.39.36 drops `Frames.Id` and makes `(EventId, FrameId)` the primary key,
  so `FrameResponse.id` is gone and `GET`/`PATCH`/`DELETE
  /api/v3/frames/{id}` become `/api/v3/frames/{event_id}/{frame_id}`.
  `GET /api/v3/frames/{id}/image` is removed; `GET
  /api/v3/events/{id}/frames/{fid}/image` serves the same image.
- **BREAKING: `Libvlc` is no longer a monitor, preset or control type.**
  ZoneMinder 1.39.38 removes the Libvlc and NVSocket capture methods and
  converts existing rows to Ffmpeg.
- **`type` on controls is accepted and ignored.** ZoneMinder 1.39.39 drops
  `Controls.Type`, which nothing ever read. Create and update still accept the
  field, so existing clients keep working, and `ControlResponse.type` is
  always `null`.
- **BREAKING: `frame_skip` is gone from monitor requests and responses.**
  ZoneMinder 1.39.24 drops `Monitors.FrameSkip`, and zm-api now mirrors that
  update, so `MonitorResponse` no longer carries the field and create/update
  ignore it. `motion_frame_skip` is unchanged.
- **BREAKING: six `operationId`s renamed** (#32). They were duplicated, which
  meant a generated client silently got one method and lost the other, so this
  had to change — but it renames methods for anyone already generating against
  the spec.
  <br>`list_models` → `list_ai_models`, `create_model` → `create_ai_model`,
  `get_model` → `get_ai_model`, `update_model` → `update_ai_model`,
  `delete_model` → `delete_ai_model` (the AI registry; the camera-model routes
  keep the plain names). `update_state` on `/monitors/{id}/state` →
  `update_monitor_state`, and on `/states/{id}` → `update_state_preset`.
  <br>The compatibility gate did not catch this on its first run — it compared
  paths, response shapes and schemas but not operation ids. It does now, which
  is how the list above was produced.
- **BREAKING: `rate_limit_per_second` renamed to `rate_limit_period_secs`**
  (#70). The old name read as a rate and meant a period. It is still accepted
  as an alias, so no configuration needs changing, but the old spelling is
  misleading enough that it should not be used in new files.

- **BREAKING: `GET /api/v3/me` returns a wrapper, not a bare user.** As of
  `5ce04e5` the response is `MeResponse` — `{ user, issued_at, expires_at,
  token_type }` — where it was previously `UserResponse` with the eight
  permission columns at the top level. This shipped without a changelog entry
  and broke zm-web's permission gating: reading the wrapper as a user finds no
  permission columns, and absent columns fail closed to `None`, so the camera
  wall and every edit control disappeared.
  <br>Clients should read `response.user`. Accepting both shapes is worth it
  while older backends are still deployed.
- **BREAKING: the project is named `zm-api` throughout, including on disk.**
  The binary is `/usr/bin/zm-api`, config lives in `/etc/zm-api/`, state in
  `/var/lib/zm-api/`, logs in `/var/log/zm-api/`, and the unit is
  `zm-api.service`; the helpers are `zm-api-db` and `zm-api-takeover`, and the
  man pages match. The distribution packages were already called `zm-api` — only
  what they installed disagreed. Nothing has been released, so there is no
  upgrade path to migrate; a pre-release install should be removed and
  reinstalled. The Rust crate is still imported as `zm_api`, which is the normal
  Cargo mapping for a hyphenated package name.
- `packaging/install.sh` rewritten for source installs: it now matches the
  package layout, installs the man pages and `zm-api-db`, and runs
  `setup-instance.sh` to generate JWT keys. Previously it installed the unit to a
  different directory than the packages, never generated keys, and left a
  freshly "installed" service unable to sign a token.
- Removed the dead `[package.metadata.rpm]` block from `Cargo.toml`; nothing
  invoked cargo-rpm, and it duplicated `packaging/rpm/zm-api.spec`.

## [3.0.0-alpha.1]

First Rust release, replacing ZoneMinder's Perl/PHP/CGI API surface with a
single native service. It talks directly to an existing ZoneMinder
MySQL/MariaDB database and ships in **passive mode**, serving only the REST API
so it can be installed alongside a running ZoneMinder without touching its
daemons.

### Added
- **REST API** under `/api/v3` for monitors, events, frames, zones, groups,
  users, storage, controls, PTZ presets, and configuration — with an
  auto-generated OpenAPI 3 spec at `/api-docs/openapi.json` and Swagger UI at
  `/swagger-ui`.
- **Live streaming** over WebRTC and HLS, sourced from zmc's per-monitor stream
  socket (video and audio on one connection with a HELLO codec handshake).
- **Event playback** — VOD, fragmented-MP4 streaming, thumbnails, and motion
  synopsis.
- **Authentication and authorisation** — RS256 JWTs with separate access and
  refresh key pairs, server-side revocation, feature-level RBAC, and row-level
  monitor ACLs.
- **Daemon supervision (takeover mode)** — one native supervisor replacing both
  `zmdc.pl` and `zmwatch.pl`, with exponential backoff, database
  reconciliation, REST daemon control, and a `zmdc.sock` compatibility shim.
  `zm-api-takeover` switches a host between modes in one command, either way.
- **Retention** — automatic recording cleanup bounded by free-space floor, age,
  and per-storage quota, deleting media and database rows together.
- **ONVIF** device discovery, media profiles, PTZ, and event pull-point.
- **Natural-language event search** over MariaDB 11.8 native vectors.
- **Object-detection registry** — CRUD over ZoneMinder 1.39's `AI_Datasets`,
  `AI_Models`, and `AI_Object_Classes`, plus the per-monitor detection columns.
- **Packaging** for Debian/Ubuntu (`.deb`), Fedora/RHEL/openSUSE (`.rpm`), and
  Arch (`PKGBUILD`), with a systemd unit, per-install JWT key generation, and
  built-in TLS/ACME.

### Notes
- Upgrading an existing ZoneMinder database requires
  `zm-api-db bridge -u mysql://...` before first start. Only a fresh, empty
  database should use `zm-api-db up`.
- Passive mode is the install-time default so the package cannot disturb a
  running ZoneMinder. Takeover is the intended destination — one native
  supervisor replacing `zmdc.pl` and `zmwatch.pl` — and `zm-api-takeover`
  switches either way in one command.
