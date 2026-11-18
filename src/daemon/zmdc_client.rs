//! A client for ZoneMinder's own `zmdc.sock`, for passive mode.
//!
//! In passive mode `zmdc.pl` supervises the daemons and zm-api runs no daemon
//! manager of its own. ZoneMinder's web UI controls and reads them through
//! `zmdc.sock`, and so does this (#155, #156). The protocol is zmdc.pl's
//! client half: write `command;daemon;arg;…`, close the write side, and read
//! the reply lines until the server closes.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::daemon::ipc::ProcessStatus;
use crate::daemon::ProcessState;

/// How long zmdc.pl's own client waits for a reply.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Send one command and return the reply text.
pub async fn request(socket: &Path, command: &str, spec: Option<&str>) -> std::io::Result<String> {
    let message = message(command, spec);
    let exchange = async {
        let mut stream = UnixStream::connect(socket).await?;
        stream.write_all(message.as_bytes()).await?;
        stream.shutdown().await?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await?;
        Ok(reply)
    };
    tokio::time::timeout(TIMEOUT, exchange)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "zmdc.sock did not reply"))?
}

/// `start;zmc;-m;1` from `start` and `zmc -m 1`.
fn message(command: &str, spec: Option<&str>) -> String {
    let mut parts = vec![command];
    if let Some(spec) = spec {
        parts.extend(spec.split_whitespace());
    }
    parts.join(";")
}

/// The processes listed in a `status` reply. zmdc.pl writes one line per
/// running process, `'zmc -m 1' running since 26/10/18 10:00:00, pid = 1234,
/// valid`, then one per pending one, `'zmc -m 2' pending at 26/10/18 …`.
pub fn parse_status(reply: &str, now: jiff::civil::DateTime) -> Vec<ProcessStatus> {
    reply
        .lines()
        .filter_map(|line| parse_line(line, now))
        .collect()
}

fn parse_line(line: &str, now: jiff::civil::DateTime) -> Option<ProcessStatus> {
    let rest = line.trim().strip_prefix('\'')?;
    let (command, rest) = rest.split_once('\'')?;
    let rest = rest.trim_start();
    let (state, pid, uptime_seconds) = if let Some(after) = rest.strip_prefix("running since ") {
        let (since, tail) = after.split_once(", pid = ").unwrap_or((after, ""));
        let pid = tail
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|p| p.parse().ok());
        let uptime = jiff::civil::DateTime::strptime("%y/%m/%d %H:%M:%S", since.trim())
            .ok()
            .and_then(|t| now.duration_since(t).as_secs().try_into().ok());
        (ProcessState::Running, pid, uptime)
    } else if rest.starts_with("pending at ") {
        (ProcessState::Restarting, None, None)
    } else {
        return None;
    };
    Some(ProcessStatus {
        id: command.to_string(),
        name: command
            .split_whitespace()
            .next()
            .unwrap_or(command)
            .to_string(),
        state,
        pid,
        uptime_seconds,
        restart_count: 0,
        monitor_id: monitor_id(command),
        last_exit_code: None,
        failure_reason: None,
    })
}

/// The monitor a command serves: `zmc -m 3`, `zmcontrol.pl --id 3`,
/// `zmtrack.pl -m 3`.
fn monitor_id(command: &str) -> Option<u32> {
    let mut words = command.split_whitespace();
    while let Some(w) = words.next() {
        if w == "-m" || w == "--id" || w == "--monitor_id" {
            return words.next()?.parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> jiff::civil::DateTime {
        jiff::civil::date(2026, 10, 18).at(10, 1, 40, 0)
    }

    #[test]
    fn messages_are_semicolon_joined_like_zmdc_pl_sends_them() {
        assert_eq!(message("status", None), "status");
        assert_eq!(message("start", Some("zmc -m 1")), "start;zmc;-m;1");
        assert_eq!(
            message("stop", Some("zmcontrol.pl --id 4")),
            "stop;zmcontrol.pl;--id;4"
        );
    }

    #[test]
    fn a_status_reply_lists_running_and_pending_processes() {
        let reply = "'zmc -m 1' running since 26/10/18 10:00:00, pid = 1234, valid\n\
                     'zmfilter.pl --filter_id=1 --daemon' running since 26/10/18 09:00:00, pid = 99, valid\n\
                     'zmc -m 2' pending at 26/10/18 10:01:30\n\
                     some log line zmdc echoed\n";
        let ps = parse_status(reply, now());
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[0].id, "zmc -m 1");
        assert_eq!(ps[0].name, "zmc");
        assert_eq!(ps[0].state, ProcessState::Running);
        assert_eq!(ps[0].pid, Some(1234));
        assert_eq!(ps[0].uptime_seconds, Some(100));
        assert_eq!(ps[0].monitor_id, Some(1));
        assert_eq!(ps[1].name, "zmfilter.pl");
        assert_eq!(ps[1].monitor_id, None);
        assert_eq!(ps[2].state, ProcessState::Restarting);
        assert_eq!(ps[2].pid, None);
    }

    #[test]
    fn a_single_daemon_status_has_no_trailing_newline_or_valid() {
        let ps = parse_status(
            "'zmc -m 7' running since 26/10/18 10:01:00, pid = 42",
            now(),
        );
        assert_eq!(ps[0].pid, Some(42));
        assert_eq!(ps[0].monitor_id, Some(7));
        assert!(parse_status("'zmc -m 7' not running\n", now()).is_empty());
    }

    #[tokio::test]
    async fn talks_to_a_socket_the_way_zmdc_pl_does() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zmdc.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut got = String::new();
            s.read_to_string(&mut got).await.unwrap();
            s.write_all(b"'zmc -m 1' running since 26/10/18 10:00:00, pid = 5\n")
                .await
                .unwrap();
            got
        });
        let reply = request(&path, "status", None).await.unwrap();
        assert_eq!(server.await.unwrap(), "status");
        assert!(reply.contains("pid = 5"));
    }
}
