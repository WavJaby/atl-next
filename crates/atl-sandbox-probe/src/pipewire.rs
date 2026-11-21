// SPDX-License-Identifier: MIT
//! `atl-sandbox-probe pipewire REMOTE SINK SOURCE`: what an app process's PipeWire client sees and may do through REMOTE (the socket the sandbox shows at the PipeWire socket's path).
//! Uses the host's own PipeWire clients, as an app's native code would use libpipewire: `pw-dump` for the graph as this client sees it (each object's permission bits), `pw-record` for a capture attempt
//! from SOURCE, `pw-play` for a playback of silence into SINK; both streams `node.dont-fallback` so a target the client cannot see fails instead of reaching another node.
//! Privacy: counts and yes/no only; no object name or property value is printed; captured bytes are counted and dropped.
#![expect(
    clippy::disallowed_types,
    reason = "a test fixture already inside the app sandbox: it runs the host's PipeWire tools as the app's own children, as app native code would"
)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a capture attempt reads: a linked stream delivers its first buffers within one quantum (1024 frames at 48 kHz = 21 ms); 1.5 s leaves room for the session manager's link.
const CAPTURE_FOR: Duration = Duration::from_millis(1500);
/// How long pw-dump / pw-play may take before the probe reports `timeout`.
const TOOL_LIMIT: Duration = Duration::from_secs(10);
/// The silence pw-play gets: 0.25 s of pw-cat's raw default (s16, 2 channels, 48 kHz) = 48000 * 2 * 2 / 4 bytes.
const SILENCE_BYTES: usize = 48_000;
/// Both streams' extra props: no fallback to the default node when the target is not visible.
const NO_FALLBACK: &str = "{ node.dont-fallback=true }";

/// The census of one pw-dump: the fields of the probe's line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Census {
    /// the client's own `pipewire.access` (from its own client object), `none` when it has none
    pub access: String,
    pub objects: usize,
    pub nodes: usize,
    pub sinks: usize,
    pub sources: usize,
    pub video: usize,
    /// nodes of no class the allow-list ever grants (Audio/Sink, Audio/Source, Video/Source): drivers, other apps' streams
    pub foreign: usize,
    /// objects other than the client's own client object with W / with M
    pub w: usize,
    pub m: usize,
    pub sink: bool,
    pub source: bool,
}

/// The census of pw-dump's JSON (an array of `{ id, type, permissions: ["r","w","x","m"...], info: { props } }`); `me` = pw-dump's own client object (application.name pw-dump).
pub fn census(dump: &Value, sink: &str, source: &str) -> Census {
    let mut c = Census {
        access: "none".into(),
        ..Census::default()
    };
    for o in dump.as_array().into_iter().flatten() {
        let ty = o["type"].as_str().unwrap_or("");
        let props = &o["info"]["props"];
        let perms: Vec<&str> = o["permissions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let me = ty == "PipeWire:Interface:Client" && props["application.name"] == "pw-dump";
        c.objects += 1;
        if me {
            if let Some(a) = props["pipewire.access"].as_str() {
                a.clone_into(&mut c.access);
            }
        } else {
            c.w += usize::from(perms.contains(&"w"));
            c.m += usize::from(perms.contains(&"m"));
        }
        if ty == "PipeWire:Interface:Node" {
            c.nodes += 1;
            let class = props["media.class"].as_str().unwrap_or("");
            let name = props["node.name"].as_str().unwrap_or("");
            let (is_sink, is_source, is_video) = (
                class.starts_with("Audio/Sink"),
                class.starts_with("Audio/Source"),
                class == "Video/Source",
            );
            c.sinks += usize::from(is_sink);
            c.sources += usize::from(is_source);
            c.video += usize::from(is_video);
            c.foreign += usize::from(!(is_sink || is_source || is_video));
            c.sink |= name == sink;
            c.source |= name == source;
        }
    }
    c
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Runs `cmd` with stdout piped; reads at most `read_for` (None: to its end, bounded by [`TOOL_LIMIT`]), then kills it. Returns (bytes read, exited ok); the bytes go to `sink`.
fn run_bounded(
    mut cmd: Command,
    read_for: Option<Duration>,
    keep: Option<&mut Vec<u8>>,
) -> Result<(usize, bool), String> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("err:{}", e.raw_os_error().unwrap_or(0)))?;
    let mut out = child.stdout.take().ok_or("err:nostdout")?;
    let limit = read_for.unwrap_or(TOOL_LIMIT);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = out.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let start = Instant::now();
    let mut n = 0;
    let mut keep = keep;
    let mut ended = false;
    while let Some(left) = limit.checked_sub(start.elapsed()) {
        match rx.recv_timeout(left) {
            Ok(b) => {
                n += b.len();
                if let Some(k) = keep.as_deref_mut() {
                    k.extend_from_slice(&b);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                ended = true;
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
        }
    }
    if !ended {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|e| format!("err:{}", e.raw_os_error().unwrap_or(0)))?;
    if !ended && read_for.is_none() {
        return Err("timeout".into());
    }
    Ok((n, ended && status.success()))
}

/// The probe's `pipewire` line: the census, then `capture=<bytes>` and `play=ok|<outcome>`.
pub fn probe(remote: &str, sink: &str, source: &str) -> String {
    let mut dump = Vec::new();
    let mut cmd = Command::new("pw-dump");
    cmd.args(["--remote", remote]);
    let census_part = match run_bounded(cmd, None, Some(&mut dump)) {
        Ok((_, true)) => match serde_json::from_slice::<Value>(&dump) {
            Ok(v) => {
                let c = census(&v, sink, source);
                format!(
                    "access={} objects={} nodes={} sinks={} sources={} video={} foreign={} w={} m={} sink={} source={}",
                    c.access,
                    c.objects,
                    c.nodes,
                    c.sinks,
                    c.sources,
                    c.video,
                    c.foreign,
                    c.w,
                    c.m,
                    yes(c.sink),
                    yes(c.source)
                )
            }
            Err(_) => "dump=err:json".into(),
        },
        Ok((_, false)) => "dump=err:exit".into(),
        Err(e) => format!("dump={e}"),
    };
    let mut rec = Command::new("pw-record");
    // pw-cat's remote is `--remote` / `-R`; its `-r` is `--rate`
    rec.args([
        "--remote",
        remote,
        "--target",
        source,
        "-P",
        NO_FALLBACK,
        "--raw",
        "-",
    ]);
    let capture = match run_bounded(rec, Some(CAPTURE_FOR), None) {
        Ok((n, _)) => n.to_string(),
        Err(e) => e,
    };
    let play = play(remote, sink);
    format!("{census_part} capture={capture} play={play}")
}

/// pw-play of [`SILENCE_BYTES`] of zeros into `sink`: `ok` once it drained and exited 0.
fn play(remote: &str, sink: &str) -> String {
    let child = Command::new("pw-play")
        .args([
            "--remote",
            remote,
            "--target",
            sink,
            "-P",
            NO_FALLBACK,
            "--raw",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return format!("err:{}", e.raw_os_error().unwrap_or(0)),
    };
    if let Some(mut i) = child.stdin.take() {
        let _ = i.write_all(&[0u8; SILENCE_BYTES]);
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return "ok".into(),
            Ok(Some(s)) => return format!("exit:{}", s.code().unwrap_or(-1)),
            Ok(None) if start.elapsed() > TOOL_LIMIT => {
                let _ = child.kill();
                let _ = child.wait();
                return "timeout".into();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return format!("err:{}", e.raw_os_error().unwrap_or(0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn census_counts_by_class_and_permission() {
        // a hand-written dump: core, own client (rwx, access atl), another client (rwxm), a granted sink, a source, a driver node with no class
        let dump = serde_json::json!([
            { "id": 0, "type": "PipeWire:Interface:Core", "permissions": ["r", "x"], "info": { "props": {} } },
            { "id": 40, "type": "PipeWire:Interface:Client", "permissions": ["r", "w", "x"], "info": { "props": { "application.name": "pw-dump", "pipewire.access": "atl" } } },
            { "id": 41, "type": "PipeWire:Interface:Client", "permissions": ["r", "w", "x", "m"], "info": { "props": { "application.name": "other" } } },
            { "id": 10, "type": "PipeWire:Interface:Node", "permissions": ["r"], "info": { "props": { "media.class": "Audio/Sink", "node.name": "s" } } },
            { "id": 20, "type": "PipeWire:Interface:Node", "permissions": ["r"], "info": { "props": { "media.class": "Audio/Source/Virtual", "node.name": "src" } } },
            { "id": 30, "type": "PipeWire:Interface:Node", "permissions": ["r", "w"], "info": { "props": { "node.name": "Dummy-Driver" } } }
        ]);
        let want = Census {
            access: "atl".into(),
            objects: 6,
            nodes: 3,
            sinks: 1,
            sources: 1,
            video: 0,
            foreign: 1,
            w: 2,
            m: 1,
            sink: true,
            source: true,
        };
        assert_eq!(census(&dump, "s", "src"), want);
    }
}
