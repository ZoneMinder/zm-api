# Multi-server installs

A ZoneMinder install can spread its cameras over several servers that share one database. Each camera belongs to one server, named by `Monitors.ServerId`.

Three things a monitor needs exist only on the server that runs it: its shared memory, the stream socket zmc serves video on, and the daemon control socket. So a zm-api can serve live video, snapshots, alarms and daemon control for its own server's monitors and no others.

## How it works

Run zm-api on every server. A client can talk to any one of them.

When a request arrives for a monitor another server runs, zm-api sends the same request to the zm-api on that server and streams the answer back. The client never needs a second address.

The relayed request carries the caller's own token. The owning server authenticates it and applies that user's permissions as it would for a direct request. No server holds a credential that can act as anyone.

What is relayed:

- live HLS, live start and stop, live stats, WHEP
- snapshots
- forcing and cancelling an alarm
- PTZ
- a monitor's state (start, stop, restart), its pipeline, and edits to and deletion of its row
- running a monitor action

- an event's recording and files (see [Recordings](#recordings))

What stays on the server the client asked: anything that only reads or writes the database, such as listing monitors, reading a monitor's row, listing events, reading or editing an event's row, tags and users.

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

## Limits

- **WebRTC media goes direct.** WHEP signalling is relayed, but the video itself travels over UDP from the server that runs the camera. The viewer must be able to reach that server.
- **WebSocket signalling is not relayed.** A WebSocket for another server's monitor answers 421 with that server's address. Use WHEP, or connect to that server.
- **System-wide control is per server.** `/api/v3/daemons` and `/api/v3/system/*` act on the server that receives them.
- **Whole-database views are not relayed.** The media job list (`/api/v3/jobs`) shows the queue this server sees: all jobs when the database holds the queue, only this server's when a file does. The range synopsis (`/api/v3/events/synopsis`) names no single event, so this server answers it.
- **A logout takes up to ten seconds to reach the other servers.** Each server re-reads the token floors from the database on that interval.
- **Rate limits see the relaying server.** A relayed request arrives from another server's address, with the client's own appended to `X-Forwarded-For`. The global limiter is off by default. If you turn it on, set `trust_proxy_headers = true` and list the other servers in `trusted_proxies`, so each client is counted on its own.

Turning relaying off with `cluster.enabled = false` restores the old behaviour, where requests for another server's monitor fail locally.
