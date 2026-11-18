# Live events

`GET /api/v3/events/stream` pushes what happens on your cameras as it happens:
events starting and ending, their notes changing, and each camera's alarm
state. Home Assistant, dashboards and detectors can react within a fraction of
a second, without ZoneMinder's MQTT and without polling `GET /events`.

It is a [Server-Sent Events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
stream: plain HTTP that stays open. Send a header token:

```
curl -N -H "Authorization: Bearer $TOKEN" https://zm.example/api/v3/events/stream
event: alarm_state
data: {"type":"alarm_state","monitor_id":1,"state":"idle","score":0}

event: event_start
id: 34735
data: {"type":"event_start","event_id":34735,"monitor_id":1,"name":"Event-34735","cause":"Motion",...}
```

The stream covers every monitor you may see. Add `?monitor_id=3` for one.

| Message | When |
|---|---|
| `alarm_state` | For every monitor when you connect, then whenever one changes: `idle`, `prealarm`, `alarm`, `alert`, or `unknown` (capture not running). Carries the last frame's `score`. |
| `event_start` | An event opens. |
| `event_end` | It closes: `end_date_time`, `frames`, `alarm_frames`, `max_score` and `length` are final. |
| `event_updated` | Its `notes` changed. Object detection (zmeventnotification, pyzm) writes results such as `detected:person:97%` there. |
| `lagged` | You fell behind and missed messages; reconnect with `Last-Event-ID`. |

Event messages carry `event_id`, `monitor_id`, `name`, `cause`, `notes`,
`start_date_time`, `end_date_time` (null while open), `frames`,
`alarm_frames`, `max_score` and `length` (seconds).

## Reconnecting

Each event message's SSE `id` is its event id. Reconnect with
`Last-Event-ID: <id>` (browsers' `EventSource` does this for you) or
`?since=<id>`, and the events after it, up to 500, are replayed before the
live ones. Alarm states can't be replayed, but every monitor's current state is
sent on connect. Expect the same event more than once across a reconnect; key
on `(event_id, type)`.

The stream ends when your access token expires and when you lose access
(logout, a disabled account, losing Events or the monitor; checked every
minute). Reconnect with a fresh token and `Last-Event-ID`. A comment line
arrives every 15 seconds, so a silent connection is a dead one.

## Several servers

On an install with more than one ZoneMinder server, one subscription covers
every server. Events are read from the shared database, so any server has
them all. Alarm states exist only on the server that runs a camera, so the
server you connect to also subscribes to each other server's stream and passes
on its `alarm_state` messages, for that server's own cameras only. Nothing is
sent twice.

- Each of your streams opens one upstream stream per other server, with your
  own token. That server applies your permissions and ends its stream when your
  token expires, as it would for you directly. This costs one connection per
  client per server.
- A server that is down does not break your stream. Its cameras send no
  `alarm_state` updates until it is back. zm-api reconnects with a wait that
  grows to 30 seconds and logs one warning per outage.
- On connect you get the current state of each camera as before, from its own
  server. After a server returns, only states that changed are sent.
- You see an `alarm_state` only for cameras you may see.
- `Last-Event-ID` and `?since=` replay events from the database, which covers
  every server. Alarm states are never replayed.
- A request that carries `X-ZM-Forwarded-By`, which is how one zm-api asks
  another, gets only that server's own alarm states and opens no further
  streams, so servers never follow each other in a loop.

A single-server install opens no upstream connections and its output is the same.

## How it works

One watcher in zm-api serves every subscriber and runs only while someone is
connected. Alarm states come from each camera's shared memory, four times a
second; a camera's new event wakes the watcher at once. Events come from
ZoneMinder's `Events` table, so cameras on other ZoneMinder servers appear too,
within a second or so. Nothing is written to the database. On a multi-server
install the watcher reads shared memory only for the cameras its own server
runs.
