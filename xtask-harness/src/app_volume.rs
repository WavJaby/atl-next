// SPDX-License-Identifier: MIT
//! `xtask test audio-app --volume`: the app volume on the app's own host stream (atl-audioflinger volume.rs), driven from both ends while the fixture plays into the gate's null sink.
//! Write: the app starts with `--app-volume-index=`[`WRITE_INDEX`] (atld, then the launcher); the node's `channelVolumes` must read cube([`WRITE_INDEX`]). Observe: the gate sets the node's `channelVolumes` to cube([`OBSERVE_INDEX`]) with `pw-cli set-param`, then mute on, then mute off; each must show in the app log as an `app-volume: observed` line with that index and mute.
//! cube(i) = (i / [`MUSIC_MAX`])^3: the desktop slider is the cube root of PipeWire's linear volume, and the app's index maps linearly onto the slider. The node is put back to 1.0, unmuted, on every exit path, so the session manager's saved volume for the fixture's app id stays what the PCM run expects.

use std::process::Command;
use std::time::{Duration, Instant};

/// AOSP's music stream maximum index, AudioService.MAX_STREAM_VOLUME[STREAM_MUSIC] (ro.config.media_vol_steps unset): written here, not read from the product, so the gate checks the product's table too.
const MUSIC_MAX: u32 = 15;
/// The written index: (9/15)^3 = 0.6^3 = 0.216.
pub(crate) const WRITE_INDEX: u32 = 9;
/// The observed index: (6/15)^3 = 0.4^3 = 0.064, a whole step (cbrt(0.064) * 15 = 6 exactly), not a half step that rounding could take either way.
const OBSERVE_INDEX: u32 = 6;
/// Volumes compared to this: pw-dump prints 6 decimals and the server stores f32 (error < 1e-7), while neighbouring steps of 15 differ by >= 0.0003 (0 -> 1/15^3); 1e-4 separates steps and absorbs printing.
const TOLERANCE: f64 = 1e-4;
/// How long the app may take to open its output (ART start, the fixture's first track).
const STREAM_LIMIT: Duration = Duration::from_secs(120);
/// How long one change may take to arrive: S0 measured 6-8 ms for a change event; this bounds a hang.
const STEP_LIMIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(50);

fn cube(index: u32) -> f64 {
    (f64::from(index) / f64::from(MUSIC_MAX)).powi(3)
}

/// The app's output node: id, its channelVolumes and mute as the server shows them.
struct Node {
    id: u64,
    volumes: Vec<f64>,
    mute: Option<bool>,
}

/// The node of `app_id`'s output stream in `pw-dump`, if it exists.
fn find(app_id: &str) -> Option<Node> {
    let out = Command::new("pw-dump").output().ok()?;
    let dump: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    let node = dump.iter().find(|o| {
        o["type"] == "PipeWire:Interface:Node"
            && o.pointer("/info/props/application.id")
                .and_then(|v| v.as_str())
                == Some(app_id)
            && o.pointer("/info/props/media.class")
                .and_then(|v| v.as_str())
                == Some("Stream/Output/Audio")
    })?;
    let props = node
        .pointer("/info/params/Props")
        .and_then(|p| p.as_array())
        .and_then(|a| a.iter().find(|p| p.get("channelVolumes").is_some()));
    Some(Node {
        id: node["id"].as_u64()?,
        volumes: props
            .and_then(|p| p["channelVolumes"].as_array())
            .map(|a| a.iter().filter_map(|v| v.as_f64()).collect())
            .unwrap_or_default(),
        mute: props.and_then(|p| p["mute"].as_bool()),
    })
}

fn set(node: u64, pod: &str) -> Result<(), String> {
    let out = Command::new("pw-cli")
        .args(["set-param", &node.to_string(), "Props", pod])
        .output()
        .map_err(|e| format!("running pw-cli: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "pw-cli set-param {node} Props {pod}: {}",
            out.status
        ))
    }
}

fn volumes_pod(v: f64, channels: usize) -> String {
    let list = vec![format!("{v:.6}"); channels.max(1)].join(", ");
    format!("{{ channelVolumes: [ {list} ] }}")
}

/// Puts the node back to 1.0, unmuted, when dropped.
struct Restore {
    node: u64,
    channels: usize,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = set(self.node, &volumes_pod(1.0, self.channels));
        let _ = set(self.node, "{ mute: false }");
    }
}

/// Waits until `done` holds or `limit` passes.
fn wait<T>(limit: Duration, mut done: impl FnMut() -> Option<T>) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = done() {
            return Some(v);
        }
        if start.elapsed() > limit {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// Runs the steps against the running app whose log is `log` (read by `read`); returns the verdicts as (name, pass, detail).
pub(crate) fn steps(app_id: &str, read: impl Fn() -> String) -> Vec<(&'static str, bool, String)> {
    let mut verdicts = Vec::new();
    let Some(node) = wait(STREAM_LIMIT, || find(app_id)) else {
        verdicts.push((
            "app volume write",
            false,
            format!(
                "no Stream/Output/Audio node with application.id={app_id} within {STREAM_LIMIT:?}"
            ),
        ));
        return verdicts;
    };
    let channels = node.volumes.len().max(1);
    let _restore = Restore {
        node: node.id,
        channels,
    };

    let want = cube(WRITE_INDEX);
    let seen = wait(STEP_LIMIT, || {
        find(app_id).filter(|n| {
            !n.volumes.is_empty() && n.volumes.iter().all(|v| (v - want).abs() < TOLERANCE)
        })
    });
    let last = find(app_id).map(|n| n.volumes).unwrap_or_default();
    verdicts.push((
        "app volume write",
        seen.is_some(),
        format!("index {WRITE_INDEX} -> node {} channelVolumes {last:?}, want each {want:.6} +-{TOLERANCE}", node.id),
    ));

    let v = cube(OBSERVE_INDEX);
    for (name, pod, mute) in [
        ("app volume observe", volumes_pod(v, channels), false),
        ("app volume mute on", "{ mute: true }".to_owned(), true),
        ("app volume mute off", "{ mute: false }".to_owned(), false),
    ] {
        let line =
            format!("app-volume: observed index={OBSERVE_INDEX} max={MUSIC_MAX} mute={mute} ");
        let before = read().len();
        let sent = set(node.id, &pod);
        let found = sent.is_ok()
            && wait(STEP_LIMIT, || {
                read()
                    .get(before..)
                    .filter(|t| t.contains(&line))
                    .map(|_| ())
            })
            .is_some();
        let state = find(app_id)
            .map(|n| format!("{:?} mute={:?}", n.volumes, n.mute))
            .unwrap_or_default();
        verdicts.push((
            name,
            found,
            format!(
                "set {pod} ({}) -> want app log `{}`; node now {state}",
                sent.err().unwrap_or_else(|| "ok".to_owned()),
                line.trim_end()
            ),
        ));
    }
    verdicts
}
