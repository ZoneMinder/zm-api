# Multi-server installs

A ZoneMinder install can spread its cameras over several servers that share one database. Each camera belongs to one server, named by `Monitors.ServerId`.

Three things a monitor needs exist only on the server that runs it: its shared memory, the stream socket zmc serves video on, and the daemon control socket. So a zm-api can serve live video, snapshots, alarms and daemon control for its own server's monitors and no others.

## How it works

Run zm-api on every server. A client can talk to any one of them.

When a request arrives for a monitor another server runs, zm-api sends the same request to the zm-api on that server and streams the answer back. The client never needs a second address.

The relayed request carries the caller's own token. The owning server authenticates it and applies that user's permissions as it would for a direct request. No server holds a credential that can act as anyone.

What is relayed:

- live HLS, live start and stop, live stats, WHEP, and the WebRTC signalling WebSocket (see [WebSockets](#websockets))
- a monitor's status (`GET /api/v3/monitor-status/{id}`), which reads shared memory
- snapshots
- forcing and cancelling an alarm
- PTZ
- a monitor's state (start, stop, restart), its pipeline, and edits to and deletion of its row
- running a monitor action

- an event's recording and files (see [Recordings](#recordings))

What stays on the server the client asked: anything that only reads or writes the database, such as listing monitors, reading a monitor's row, listing events, reading or editing an event's row, tags and users.

## WebSockets

`GET /api/v3/live/{monitor_id}/webrtc/ws` carries WebRTC signalling. For another server's monitor, this server checks that you may see the monitor, then opens its own WebSocket to the same path and query on the owning server, with your `Authorization` header and any `Sec-WebSocket-Protocol` you offered. Only then does it accept your upgrade, so the answers are ordinary HTTP errors:

- the owning server is down: 502
- the owning server refuses (404, 403, 503, ...): the same status and body come back to you
- the servers have different JWT keys: 502

After that, messages are copied one at a time in both directions (text, binary, ping, pong and close, with the close code and reason). When either side closes or fails, the other is closed. A failure on the owning server's side closes your socket with code 1011. The relay sets no idle timeout, since a signalling socket can sit quiet for a long time.

WebRTC media still travels direct from the owning server to the viewer over UDP. The viewer must be able to reach that server. Only the signalling goes through this one.

For `wss://` the relay trusts the system's root certificates. If `cluster.tls_ca_file` is set it trusts that file's certificates alone for WebSockets, because the WebSocket client can't add to the system roots the way the HTTP client does. Put every CA the peers need in that file.

A request that only claims to upgrade, without a valid WebSocket handshake, answers 421.

## The live event stream

`GET /api/v3/events/stream` covers every server. Events come from the shared database, so any server has them all. Alarm states come from shared memory, which only the server that runs a monitor has. So the server you connect to also follows each other server's stream for that server's alarm states, and merges them into yours. Each of your own streams opens one stream to each other server, with your own token, so the other server applies your permissions and ends the stream when your token expires. Your server shows you an alarm state only for monitors you may see and only from the server that runs the monitor. If a server is down, its monitors send no alarm states until it is back; the rest of the stream is not affected. See [Live events](live-events.md#several-servers).

## Recordings

ZoneMinder gives each storage area an optional `ServerId`:

- **No `ServerId` (or 0): shared storage.** Every server can read and delete its files, over NFS for example. Requests for these events are answered by whichever server was asked.
- **A `ServerId`: local storage.** Only that server can read its files. A request for one of its events' files goes to that server.

Relayed for an event on another server's storage: playback and HLS, the event's video, thumbnail and frame images, event info, synopsis, generated videos (list, create, download, delete), and `DELETE /api/v3/events/{id}`, so the server that has the files removes them. This server first checks that the caller may see the event's monitor.

Not relayed, because the database holds all they need: the event list, an event's row and its edits, tags, notes, and the signed link for playback (the link is valid on every server, and the requests that use it are relayed).

Background jobs leave another server's storage alone. The retention reaper, the maintenance audit (both its database and its filesystem checks) and the event audit report skip a storage area that names another server, and the log says so once. Each server's own jobs cover its own storage, so run them on every server. A media job for an event whose files are on another server stays queued until that server's worker takes it. The storage list shows no disk figures for another server's area, since the path on this server is not that disk.

Several events in one request are not split across servers:

- `POST /api/v3/events/export` packs the files of the events on storage this server can read. For an event on another server's storage it adds the details (`event.json`, `frames.json`) and a `files-unavailable.txt` naming the server. Ask that server for the files.

An event with no storage id is treated as shared.

## Setup

1. **Give every server the same JWT keys.** Copy the four key files named under `[secret]` from one server to the others. A token issued by one server must verify on all of them. This also makes signed media links valid on every server.
2. **Check each server knows who it is.** zm-api reads `ZM_SERVER_ID`, `ZM_SERVER_NAME` or `ZM_SERVER_HOST` from `zm.conf`, as ZoneMinder does. Its log says `This host is Servers.Id N` at startup.
3. **Check the servers can reach each other.** By default a server expects to find another one at its own scheme and port on that server's `Servers.Hostname`. If that is wrong for a server, name its address:

```toml
[cluster]
peer_port = 8443          # if the others listen on a different port than this one

[cluster.peers]
2 = "https://zm2.lan:8443"
```

`GET /api/v3/servers` shows the address each server was resolved to, as `api_url`, and marks the one that answered with `is_this_server`.

If the servers use certificates from a private CA, point `cluster.tls_ca_file` at its PEM file.

## When something is wrong

| Answer | Meaning |
|---|---|
| 502 `server 2 (zm2) did not answer` | The owning server is down or unreachable from this one. |
| 502 `refused credentials this server accepted` | The servers have different JWT keys. Copy the key files. |
| 421 `belongs to server 2, which has no address` | The `Servers` row has no hostname and `cluster.peers` names none. |
| 421 `the servers disagree` | Two servers each think the other runs the monitor. Check `ZM_SERVER_ID` on both. |

## Daemons and system control

`/api/v3/daemons` and `/api/v3/system/*` act on one server's daemons. Add `?server_id=` to say which:

| `server_id` | What happens |
| --- | --- |
| absent | This server, as before. |
| a number | That server. A request for another server is relayed to its zm-api with your own token, and its answer comes back unchanged. An unknown number answers 404, and a server with no address answers 421. |
| `all` | Every server, at once. The answer is 207 with one entry per server: `server_id`, `name`, `ok`, `status`, and either `result` (what that server answered) or `error`. One server being down does not fail the request, so check each entry. This server is first. |

The permission check happens here before anything is relayed, and the other server checks it again. A request that another server relayed is always handled where it lands, whatever `server_id` says.

On an install with one server, `all` answers with a single entry whose `server_id` is `null`, and any number answers 404.

### Applying a run state

`POST /api/v3/system/state` changes monitor rows for every server in the shared database, so it reaches every server's daemons without a `server_id`. This server writes the rows once, then tells each other server to restart the daemons of its own monitors that the state names. It sends `{"state_name": ..., "daemons_only": true}`, which restarts daemons and writes nothing, so sending it twice is harmless. The response keeps its usual fields for this server and adds `servers`, one entry per other server in the same shape as above. A server that cannot be reached does not undo the change: its entry says `ok: false`, and its daemons pick up the new state at their next restart.

Run states are not aimed at one server. `server_id=all` is the same as leaving it out, and a server's number answers 400.

## Live sessions and sources

`GET /api/v3/live/sessions` and `GET /api/v3/live/sources` list one server's live sessions and sources. They take `?server_id=<n>|all` exactly as the daemon routes do (see above): a number is relayed, and `all` answers 207 with one entry per server.

## Limits

- **WebRTC media goes direct.** WHEP and WebSocket signalling are relayed, but the video itself travels over UDP from the server that runs the camera. The viewer must be able to reach that server.
- **The monitor status list has no live fields for other servers' monitors.** `GET /api/v3/monitor-status` fills `alarm_state` and the other shared-memory fields only for this server's monitors. Ask for one monitor (`/monitor-status/{id}`) to have it answered by its server, or use the [live event stream](live-events.md#several-servers).
- **The event stream follows the servers it sees when it opens.** A server added later is picked up by the next connection.
- **Whole-database views are not relayed.** The media job list (`/api/v3/jobs`) shows the queue this server sees: all jobs when the database holds the queue, only this server's when a file does. The range synopsis (`/api/v3/events/synopsis`) names no single event, so this server answers it.
- **A logout takes up to ten seconds to reach the other servers.** Each server re-reads the token floors from the database on that interval.
- **Rate limits see the relaying server.** A relayed request arrives from another server's address, with the client's own appended to `X-Forwarded-For`. The global limiter is off by default. If you turn it on, set `trust_proxy_headers = true` and list the other servers in `trusted_proxies`, so each client is counted on its own.

Turning relaying off with `cluster.enabled = false` restores the old behaviour, where requests for another server's monitor fail locally.
