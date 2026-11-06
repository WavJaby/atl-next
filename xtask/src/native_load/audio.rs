// SPDX-License-Identifier: MIT
//! Stage 9 of `xtask test native-load` (audio M1): three game clips decoded through the NDK extractor and codec in the app process (the shim's `decode`), checked against values read from each container, never from the decoder.
//! Clips come from the game APKs under the workspace's `apk/` (`--apks` overrides): third-party assets, never in the repo; each run extracts them into the stage's work dir.
//! Oracle: Ogg Vorbis frames = the last page's granule position (PCM samples per channel up to the stream's end), channels and rate from the identification header. WAV: the fmt and data chunks, and raw PCM output equals the data chunk byte for byte.
//! `--break-media` decodes cheer.ogg truncated to half its bytes against the full file's expectation: the stage must fail.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

use super::{Env, Fixture};

/// A clip the gate decodes: its file name in the work dir, the APK holding it (relative to the apk root), and its entry in that APK.
struct Clip {
    name: &'static str,
    apk: &'static str,
    entry: &'static str,
}

const CLIPS: [Clip; 3] = [
    Clip {
        name: "cheer.ogg",
        apk: "happy-jump/1.12.2-x86_64/happy-jump.apk",
        entry: "assets/audio/cheer.ogg",
    },
    Clip {
        name: "game_music.ogg",
        apk: "cut-the-rope/3.63.0-x86_64/cut-the-rope-3.63.0-x86_64.apk",
        entry: "assets/sound/game_music.ogg",
    },
    Clip {
        name: "tap.wav",
        apk: "cut-the-rope/3.63.0-x86_64/cut-the-rope-3.63.0-x86_64.apk",
        entry: "assets/sounds/tap.wav",
    },
];

/// What a clip's container says its decode produces.
struct Expected {
    channels: u64,
    rate: u64,
    frames: u64,
    /// WAV only: the data chunk, which raw PCM output must equal.
    wav_data: Option<Vec<u8>>,
}

/// Ogg Vorbis: channels and rate from the identification header (the first packet), frames from the last page that carries a granule position (the stream's end in PCM samples per channel).
fn ogg(bytes: &[u8]) -> Result<Expected> {
    const NO_GRANULE: u64 = u64::MAX;
    let (mut pos, mut ident, mut last) = (0usize, None, None);
    while pos < bytes.len() {
        ensure!(
            bytes.len() >= pos + 27 && &bytes[pos..pos + 4] == b"OggS",
            "no Ogg page at byte {pos}"
        );
        let granule = u64::from_le_bytes(bytes[pos + 6..pos + 14].try_into()?);
        let nseg = bytes[pos + 26] as usize;
        ensure!(bytes.len() >= pos + 27 + nseg, "truncated segment table at byte {pos}");
        let data_len: usize = bytes[pos + 27..pos + 27 + nseg].iter().map(|&s| s as usize).sum();
        let start = pos + 27 + nseg;
        ensure!(bytes.len() >= start + data_len, "truncated page at byte {pos}");
        if ident.is_none() {
            let p = &bytes[start..start + data_len];
            ensure!(
                p.len() >= 16 && p[0] == 1 && &p[1..7] == b"vorbis",
                "first packet is not a Vorbis identification header"
            );
            ident = Some((u64::from(p[11]), u64::from(u32::from_le_bytes(p[12..16].try_into()?))));
        }
        if granule != NO_GRANULE {
            last = Some(granule);
        }
        pos = start + data_len;
    }
    let (channels, rate) = ident.context("no Ogg pages")?;
    let frames = last.context("no page with a granule position")?;
    Ok(Expected { channels, rate, frames, wav_data: None })
}

/// RIFF WAVE, PCM: channels and rate from fmt, frames = data bytes / (channels x bits / 8).
fn wav(bytes: &[u8]) -> Result<Expected> {
    ensure!(
        bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "not a RIFF WAVE file"
    );
    let (mut pos, mut fmt, mut data) = (12usize, None, None);
    while pos + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        let body = &bytes[pos + 8..(pos + 8 + size).min(bytes.len())];
        match &bytes[pos..pos + 4] {
            b"fmt " => {
                ensure!(body.len() >= 16, "short fmt chunk");
                let tag = u16::from_le_bytes(body[0..2].try_into()?);
                let channels = u16::from_le_bytes(body[2..4].try_into()?);
                let rate = u32::from_le_bytes(body[4..8].try_into()?);
                let bits = u16::from_le_bytes(body[14..16].try_into()?);
                fmt = Some((tag, channels, rate, bits));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        pos += 8 + size + (size & 1);
    }
    let (tag, channels, rate, bits) = fmt.context("no fmt chunk")?;
    ensure!(tag == 1, "not PCM: format tag {tag}");
    let data = data.context("no data chunk")?;
    let frame = usize::from(channels) * usize::from(bits) / 8;
    ensure!(frame > 0 && data.len() % frame == 0, "data chunk is not whole frames");
    Ok(Expected {
        channels: u64::from(channels),
        rate: u64::from(rate),
        frames: (data.len() / frame) as u64,
        wav_data: Some(data.to_vec()),
    })
}

fn expected(clip: &Clip, bytes: &[u8]) -> Result<Expected> {
    if clip.name.ends_with(".ogg") {
        ogg(bytes).with_context(|| clip.name)
    } else {
        wav(bytes).with_context(|| clip.name)
    }
}

/// The clip's bytes from its APK (a zip; `unzip -p` writes the entry to stdout).
fn asset(apks: &Path, clip: &Clip) -> Result<Vec<u8>> {
    let apk = apks.join(clip.apk);
    let out = Command::new("unzip")
        .arg("-p")
        .arg(&apk)
        .arg(clip.entry)
        .output()
        .context("running unzip")?;
    ensure!(
        out.status.success() && !out.stdout.is_empty(),
        "{} has no {} ({})",
        apk.display(),
        clip.entry,
        out.status
    );
    Ok(out.stdout)
}

/// The shim's `nlt decode` line as key=value pairs. An `error=` token (its own text may contain spaces) fails the clip.
fn decode(env: &Env, fixture: &Fixture, input: &Path, output: &Path) -> Result<(String, BTreeMap<String, String>), String> {
    // NativeLoad.java: the runtime and the shim load first (a[1], a[2]), then the clip and the PCM output (a[3], a[4])
    let args = [
        env.runtime.display().to_string(),
        fixture.shim.display().to_string(),
        input.display().to_string(),
        output.display().to_string(),
    ];
    let run = env
        .binder_art(fixture, "decode", &args)
        .map_err(|e| format!("{e:#}"))?;
    let line = run
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("nlt decode "))
        .map(str::to_owned)
        .ok_or_else(|| {
            let why = run
                .stderr
                .lines()
                .find(|l| l.contains("Check failed") || l.contains(" F "))
                .unwrap_or("no decode line");
            format!("no decode line, {}: {}", run.status, why.trim())
        })?;
    let pairs = line
        .split_whitespace()
        .filter_map(|t| t.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    Ok((line, pairs))
}

/// One clip's verdict: the decode's frames, channels, rate, PCM encoding and byte count against the container's; the store the process saw; raw output equal to the WAV data chunk.
fn check(name: &str, exp: &Expected, line: &str, got: &BTreeMap<String, String>, pcm: &[u8]) -> Result<String, String> {
    if got.contains_key("error") {
        return Err(format!("{name}: {line}"));
    }
    let num = |key: &str| -> Result<u64, String> {
        got.get(key)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("{name}: no {key} in `{line}`"))
    };
    let (frames, bytes, channels, rate) = (num("frames")?, num("bytes")?, num("channels")?, num("rate")?);
    let pcm_enc = num("pcm")?;
    let mut bad = Vec::new();
    if frames != exp.frames {
        bad.push(format!("frames {frames} != container {}", exp.frames));
    }
    if channels != exp.channels || rate != exp.rate {
        bad.push(format!("{channels}ch {rate}Hz != container {}ch {}Hz", exp.channels, exp.rate));
    }
    if pcm_enc != 2 {
        bad.push(format!("pcm encoding {pcm_enc}, expected 16-bit (2)"));
    }
    if bytes != frames * channels * 2 || pcm.len() as u64 != bytes {
        bad.push(format!("bytes {bytes} (file {}) != frames x channels x 2", pcm.len()));
    }
    if num("declared")? != 1 || !got.get("instances").is_some_and(|i| i.split(',').any(|n| n == "default")) {
        bad.push(format!("store not declared with instance default: `{line}`"));
    }
    if got.get("store").map(String::as_str) != Some("local") {
        bad.push(format!("store is {:?}, expected local", got.get("store")));
    }
    if let Some(wav) = &exp.wav_data {
        if wav.as_slice() != pcm {
            bad.push(format!("PCM differs from the WAV data chunk ({} vs {} bytes)", pcm.len(), wav.len()));
        }
    }
    if !bad.is_empty() {
        return Err(format!("{name}: {}", bad.join("; ")));
    }
    Ok(format!("{name} {frames} frames {channels}ch {rate}Hz"))
}

/// Stage 9: every clip decoded and checked; Ok names the clips, Err the first clip that failed. `work` is the stage's directory.
pub fn run(env: &Env, fixture: &Fixture, apks: &Path, work: &Path, break_media: bool) -> Result<String, String> {
    std::fs::create_dir_all(work).map_err(|e| format!("{}: {e}", work.display()))?;
    let mut parts = Vec::new();
    for clip in &CLIPS {
        let full = asset(apks, clip).map_err(|e| format!("{e:#}"))?;
        let exp = expected(clip, &full).map_err(|e| format!("{e:#}"))?;
        let truncated = break_media && clip.name == "cheer.ogg";
        let input_bytes = if truncated { &full[..full.len() / 2] } else { &full[..] };
        let input = work.join(clip.name);
        std::fs::write(&input, input_bytes).map_err(|e| format!("{}: {e}", input.display()))?;
        let output = work.join(format!("{}.pcm", clip.name));
        let (line, got) = decode(env, fixture, &input, &output)?;
        let pcm = std::fs::read(&output).map_err(|e| format!("{}: {e}", output.display()))?;
        parts.push(check(clip.name, &exp, &line, &got, &pcm)?);
    }
    Ok(parts.join("; "))
}
