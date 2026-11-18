//! The live event stream across servers.
//!
//! `GET /api/v3/events/stream` carries two kinds of message. Events come from
//! the `Events` table, which every server shares, so any server has them all.
//! Alarm states come from shared memory, which only the server that runs a
//! monitor has. So a stream served here follows each other server's stream
//! for the one thing it cannot produce itself: that server's alarm states.
//!
//! Each client stream opens its own upstream stream to each other server,
//! with that client's own token. The peer applies the client's permissions
//! and ends the stream when the token expires, exactly as for a direct
//! connection, and no server holds a credential that can act as anyone. The
//! price is one connection per client per server, which the small client
//! counts of a camera system can bear.
//!
//! The upstream request carries [`FORWARDED_BY`]. A server that sees it
//! serves its own alarm states and nothing else, so servers never follow
//! each other in a loop and no event arrives twice.

use std::collections::HashMap;
use std::time::Duration;

use axum::http::{header, HeaderMap};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::forward::FORWARDED_BY;
use super::Peer;
use crate::error::AppResult;
use crate::server::state::AppState;
use crate::service::event_stream::AlarmInfo;

/// The wait before the first retry of a stream that failed, doubled after
/// each failure.
const FIRST_RETRY: Duration = Duration::from_secs(1);
/// The longest wait between retries.
const LONGEST_RETRY: Duration = Duration::from_secs(30);
/// The most one unfinished SSE message may hold. A peer that sends more is
/// not speaking the stream protocol.
const MAX_MESSAGE: usize = 256 * 1024;

/// An alarm state a peer sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAlarm {
    pub server_id: u32,
    pub alarm: AlarmInfo,
}

/// The upstream streams of one client stream. Dropping it closes them.
pub struct PeerStreams {
    rx: mpsc::Receiver<PeerAlarm>,
    tasks: Vec<JoinHandle<()>>,
}

impl PeerStreams {
    /// The next alarm state from any peer. `None` when every upstream has
    /// ended for good (the peers refused the client's credentials).
    pub async fn next(&mut self) -> Option<PeerAlarm> {
        self.rx.recv().await
    }
}

impl Drop for PeerStreams {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Open the upstream streams for a client stream, or `None` when there is
/// nothing to follow: a single-server install, a request another server
/// relayed, or no other server with an address.
pub async fn follow_peers(
    state: &AppState,
    caller: &HeaderMap,
    monitor: Option<u32>,
) -> AppResult<Option<PeerStreams>> {
    if !state.cluster.relays() || caller.contains_key(&FORWARDED_BY) {
        return Ok(None);
    }
    let (Some(this_server), Some(auth)) = (
        state.cluster.this_server(),
        caller.get(header::AUTHORIZATION).cloned(),
    ) else {
        return Ok(None);
    };
    let peers: Vec<Peer> = state
        .cluster
        .other_servers(state.db())
        .await?
        .into_iter()
        .filter_map(|member| member.peer)
        .collect();
    if peers.is_empty() {
        return Ok(None);
    }

    let (tx, rx) = mpsc::channel(256);
    let tasks = peers
        .into_iter()
        .map(|peer| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, auth.clone());
            headers.insert(header::ACCEPT, "text/event-stream".parse().expect("static"));
            headers.insert(FORWARDED_BY, this_server.into());
            let url = stream_url(&peer, monitor);
            tokio::spawn(follow(
                state.cluster.client().clone(),
                peer,
                url,
                headers,
                tx.clone(),
            ))
        })
        .collect();
    Ok(Some(PeerStreams { rx, tasks }))
}

fn stream_url(peer: &Peer, monitor: Option<u32>) -> String {
    let mut url = format!("{}/api/v3/events/stream", peer.base_url);
    if let Some(monitor) = monitor {
        url.push_str(&format!("?monitor_id={monitor}"));
    }
    url
}

/// How a stream to a peer ended.
enum Ended {
    /// Retrying would not help: the peer refused the credentials, or nobody
    /// is listening any more.
    Stop,
    /// Try again. `connected` is whether the peer had accepted the stream.
    Retry { connected: bool, why: String },
}

/// Keep one upstream stream open, reconnecting with a growing wait. Logs at
/// warn once per outage, not once per attempt.
async fn follow(
    client: reqwest::Client,
    peer: Peer,
    url: String,
    headers: HeaderMap,
    tx: mpsc::Sender<PeerAlarm>,
) {
    // Across reconnects, so the snapshot a peer sends again on each
    // connection only passes on what changed while the stream was down.
    let mut last: HashMap<u32, AlarmInfo> = HashMap::new();
    let mut delay = FIRST_RETRY;
    let mut warned = false;
    loop {
        match read_once(&client, &peer, &url, &headers, &tx, &mut last).await {
            Ended::Stop => return,
            Ended::Retry { connected, why } => {
                if connected {
                    warned = false;
                    delay = FIRST_RETRY;
                }
                if !warned {
                    warn!(
                        "{} is not sending alarm states ({why}); retrying until it does",
                        peer.label()
                    );
                    warned = true;
                } else {
                    debug!("{} still not sending alarm states ({why})", peer.label());
                }
                tokio::time::sleep(delay).await;
                delay = next_delay(delay);
            }
        }
    }
}

fn next_delay(delay: Duration) -> Duration {
    (delay * 2).min(LONGEST_RETRY)
}

async fn read_once(
    client: &reqwest::Client,
    peer: &Peer,
    url: &str,
    headers: &HeaderMap,
    tx: &mpsc::Sender<PeerAlarm>,
    last: &mut HashMap<u32, AlarmInfo>,
) -> Ended {
    let answer = match client.get(url).headers(headers.clone()).send().await {
        Ok(answer) => answer,
        // The URL can carry a monitor filter, not a token, but is left out
        // as it is everywhere else.
        Err(e) => {
            return Ended::Retry {
                connected: false,
                why: e.without_url().to_string(),
            }
        }
    };
    let status = answer.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        warn!(
            "{} refused credentials this server accepted: every server must use the same JWT \
             key files",
            peer.label()
        );
        return Ended::Stop;
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        debug!("{} gives this caller no Events access", peer.label());
        return Ended::Stop;
    }
    if !status.is_success() {
        return Ended::Retry {
            connected: false,
            why: format!("answered {status}"),
        };
    }

    let mut parser = SseParser::default();
    let mut body = answer;
    loop {
        match body.chunk().await {
            Ok(Some(chunk)) => {
                let messages = match parser.push(&chunk) {
                    Ok(messages) => messages,
                    Err(why) => {
                        return Ended::Retry {
                            connected: true,
                            why: why.to_string(),
                        }
                    }
                };
                for message in messages {
                    let Some(alarm) = alarm_state(&message) else {
                        continue;
                    };
                    if last.get(&alarm.monitor_id) == Some(&alarm) {
                        continue;
                    }
                    last.insert(alarm.monitor_id, alarm.clone());
                    let sent = tx
                        .send(PeerAlarm {
                            server_id: peer.id,
                            alarm,
                        })
                        .await;
                    if sent.is_err() {
                        return Ended::Stop;
                    }
                }
            }
            // The peer ended the stream: its client's token expired, or it
            // restarted. A reconnect with an expired token is refused, which
            // stops this task.
            Ok(None) => {
                return Ended::Retry {
                    connected: true,
                    why: "the stream ended".to_string(),
                }
            }
            Err(e) => {
                return Ended::Retry {
                    connected: true,
                    why: e.without_url().to_string(),
                }
            }
        }
    }
}

/// One Server-Sent Event: its `event` name and its `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseMessage {
    pub event: String,
    pub data: String,
}

/// The alarm state in a message, if it is one.
fn alarm_state(message: &SseMessage) -> Option<AlarmInfo> {
    if message.event != "alarm_state" {
        return None;
    }
    serde_json::from_str(&message.data).ok()
}

/// Splits a Server-Sent Events byte stream into messages. Comments (the
/// heartbeat) and fields it doesn't use are dropped.
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    /// Add bytes; return the messages they completed. An error when one
    /// message grows past [`MAX_MESSAGE`].
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseMessage>, &'static str> {
        // Line ends are LF, CR LF or CR; only the first two are sent here.
        self.buffer.extend(bytes.iter().filter(|b| **b != b'\r'));
        let mut out = Vec::new();
        while let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let block: Vec<u8> = self.buffer.drain(..end + 2).collect();
            if let Some(message) = parse_block(&String::from_utf8_lossy(&block)) {
                out.push(message);
            }
        }
        if self.buffer.len() > MAX_MESSAGE {
            return Err("a message was too large");
        }
        Ok(out)
    }
}

fn parse_block(block: &str) -> Option<SseMessage> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            // An empty field name is a comment.
            _ => {}
        }
    }
    Some(SseMessage {
        event: event?,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_split_across_chunks_and_comments_are_dropped() {
        let mut parser = SseParser::default();
        assert!(parser
            .push(b": keep-alive\n\nevent: alarm_st")
            .unwrap()
            .is_empty());
        let out = parser
            .push(b"ate\ndata: {\"monitor_id\":3,\"state\":\"alarm\",\"score\":9}\n\nevent: x\n")
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, "alarm_state");
        let alarm = alarm_state(&out[0]).unwrap();
        assert_eq!(
            alarm,
            AlarmInfo {
                monitor_id: 3,
                state: "alarm".into(),
                score: 9
            }
        );
        // The unfinished one completes later; CR LF line ends work.
        let out = parser.push(b"id: 4\r\ndata: 1\r\n\r\n").unwrap();
        assert_eq!(out[0].event, "x");
        assert!(alarm_state(&out[0]).is_none());
    }

    #[test]
    fn only_alarm_states_are_taken() {
        let event = SseMessage {
            event: "event_start".into(),
            data: r#"{"monitor_id":3,"state":"alarm","score":9}"#.into(),
        };
        assert!(alarm_state(&event).is_none());
        let broken = SseMessage {
            event: "alarm_state".into(),
            data: "not json".into(),
        };
        assert!(alarm_state(&broken).is_none());
    }

    #[test]
    fn a_runaway_message_is_an_error() {
        let mut parser = SseParser::default();
        let big = vec![b'a'; MAX_MESSAGE + 1];
        assert!(parser.push(&big).is_err());
    }

    #[test]
    fn retries_back_off_to_a_cap() {
        let mut delay = FIRST_RETRY;
        let mut seen = Vec::new();
        for _ in 0..8 {
            seen.push(delay.as_secs());
            delay = next_delay(delay);
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 30, 30, 30]);
    }
}
