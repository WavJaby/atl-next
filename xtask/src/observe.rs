// SPDX-License-Identifier: MIT
//! `remote run-apk --observe` / `--shot`: while the app holds on the target, watch its Hyprland session and shoot the app's window only.
//!
//! Each tick is one target call (`Watch::poll`, through `Target::run_bounded`) that prints the session's one Hyprland instance, the target's clock, the app's first present stamp in this run's log, and `hyprctl -j clients`. The `atl.*` clients of each tick go to `observe.jsonl`; a shot needs the one mapped, visible one (`matched`).
//! A shot is three calls: the tick's rect, `grim -g` of it to stdout, and the rect again. The PNG is kept only if the rect is unchanged across the grab, so a window that moved mid-capture never leaves desktop pixels in a shot.
//! The run's root is removed before the transfer starts, so every present line read belongs to this run.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{atld_run, remote, run_apk};

/// One observation per tick. A tick is one target call (~0.3 s on a LAN), so a shot lands within about one tick of its due time; 1 s keeps the target's sshd log short.
const TICK: Duration = Duration::from_secs(1);
/// Bound of one target call. The ssh ConnectTimeout is 20 s: a call still running after that has hung, and the run's wait loop keeps its own limit.
const CALL_LIMIT: Duration = Duration::from_secs(20);
/// A shot still missing this long after its due time is given up with its last reason: ten ticks for a window that is still mapping or settling.
const SHOT_GRACE_MS: u64 = 10_000;
/// The window class prefix: the composer names the app's `xdg_toplevel` app_id `atl.<package>` before its first frame.
const CLASS_PREFIX: &str = "atl.";
/// The first eight bytes of a PNG, which `grim` prints for a shot.
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The probe of one tick (sh on the target). `@BASE@`, `@APP@` and `@PRESENT@` are filled in by [`probe_script`]. The Hyprland instance is the one dir under the session's `hypr/`: any other count is no session.
const PROBE: &str = r#"cd @BASE@ || exit 2
RT=/run/user/$(id -u)
n=0
sig=
for d in "$RT"/hypr/*/; do
  [ -d "$d" ] || continue
  n=$((n + 1))
  sig=${d%/}
  sig=${sig##*/}
done
[ "$n" -eq 1 ] || { echo "session none $n"; exit 0; }
command -v hyprctl >/dev/null 2>&1 || { echo "session no-hyprctl"; exit 0; }
echo "session ok $sig"
echo "t $(date +%s%3N)"
stamp=$(grep -m1 '@PRESENT@' "$PWD/out/@APP@" 2>/dev/null)
set -- $stamp
echo "present ${1:-}"
echo clients-begin
HYPRLAND_INSTANCE_SIGNATURE="$sig" XDG_RUNTIME_DIR="$RT" hyprctl -j clients || exit 3
echo clients-end
"#;

/// Where the observation looks on the target. `base` is the bundle dir the probe `cd`s to; `root` and `app_log` are under its `out/`.
pub struct Plan {
    pub base: String,
    /// The run's root under `out/`, removed before the transfer.
    pub root: String,
    /// The app's log under `out/`: `<root>/log/<package>.log`.
    pub app_log: String,
    /// Shot times in ms after the app's first present, ascending.
    pub shots_ms: Vec<u64>,
}

/// A `hyprctl -j clients` entry, reduced to what the observation keeps. `at` and `size` are in layout pixels, the space `grim -g` reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Client {
    pub class: String,
    pub mapped: bool,
    pub hidden: bool,
    pub floating: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub workspace: String,
}

#[derive(Deserialize)]
struct RawClient {
    #[serde(default)]
    class: String,
    #[serde(default)]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    floating: bool,
    #[serde(default)]
    at: [i32; 2],
    #[serde(default)]
    size: [i32; 2],
    #[serde(default)]
    workspace: RawWorkspace,
}

#[derive(Default, Deserialize)]
struct RawWorkspace {
    #[serde(default)]
    name: String,
}

/// The `atl.*` clients of a `hyprctl -j clients` reply, in Hyprland's order. Fields not kept are ignored.
pub fn atl_clients(json: &str) -> Result<Vec<Client>> {
    let raw: Vec<RawClient> =
        serde_json::from_str(json).context("hyprctl -j clients: not a JSON array of clients")?;
    Ok(raw
        .into_iter()
        .filter(|c| c.class.starts_with(CLASS_PREFIX))
        .map(|c| Client {
            class: c.class,
            mapped: c.mapped,
            hidden: c.hidden,
            floating: c.floating,
            at: c.at,
            size: c.size,
            workspace: c.workspace.name,
        })
        .collect())
}

/// The window a shot may be taken of: the one mapped, visible `atl.*` client. None, or several, is no match, and then nothing is shot.
pub fn matched(clients: &[Client]) -> Result<&Client, String> {
    let shown: Vec<&Client> = clients.iter().filter(|c| c.mapped && !c.hidden).collect();
    match shown.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!(
            "no mapped atl.* window ({} atl.* clients)",
            clients.len()
        )),
        many => Err(format!("{} mapped atl.* windows, want 1", many.len())),
    }
}

/// The `grim -g` argument of a client's rect (`x,y WxH`). An empty rect is refused: it is no window to shoot.
pub fn grim_geometry(c: &Client) -> Result<String> {
    ensure!(
        c.size[0] > 0 && c.size[1] > 0,
        "{}: empty rect {:?}",
        c.class,
        c.size
    );
    Ok(format!(
        "{},{} {}x{}",
        c.at[0], c.at[1], c.size[0], c.size[1]
    ))
}

/// One tick's observation.
#[derive(Debug, PartialEq)]
pub struct Tick {
    /// The Hyprland instance: its dir name under `/run/user/<uid>/hypr`.
    pub sig: String,
    /// The target's clock, ms since the epoch.
    pub t_ms: u64,
    /// The app's first present stamp in this run's log, on the target's clock.
    pub present_ms: Option<u64>,
    pub clients: Vec<Client>,
}

/// The target's answer to one tick: its session, or why there is none to watch.
#[derive(Debug, PartialEq)]
pub enum Reply {
    NoSession(String),
    Tick(Tick),
}

/// Reads [`PROBE`]'s output: `session ...`, `t <ms>`, `present <stamp>` (empty when not seen), then the JSON between `clients-begin` and `clients-end`.
pub fn parse_reply(text: &str) -> Result<Reply> {
    let mut lines = text.lines();
    let head: Vec<&str> = lines.next().unwrap_or("").split_whitespace().collect();
    let sig = match head.as_slice() {
        ["session", "none", n] => return Ok(Reply::NoSession(no_session(n))),
        ["session", "no-hyprctl"] => {
            return Ok(Reply::NoSession(
                "hyprctl is not on the target's PATH".to_owned(),
            ));
        }
        ["session", "ok", sig] => *sig,
        _ => bail!("the target's first line is not a session line: {head:?}"),
    };
    ensure!(
        sig.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "Hyprland instance name {sig:?} has other characters"
    );
    let t_ms: u64 = field(lines.next(), "t")?
        .parse()
        .context("the target's clock")?;
    let stamp = field(lines.next(), "present")?;
    let present_ms = match stamp {
        "" => None,
        stamp => Some(
            atld_run::parse_stamp(stamp).with_context(|| format!("the present stamp {stamp:?}"))?,
        ),
    };
    ensure!(
        lines.next() == Some("clients-begin"),
        "the reply has no clients-begin line"
    );
    let mut json = String::new();
    loop {
        match lines.next() {
            Some("clients-end") => break,
            Some(line) => {
                json.push_str(line);
                json.push('\n');
            }
            None => bail!("the reply ends inside the clients list: no clients-end"),
        }
    }
    Ok(Reply::Tick(Tick {
        sig: sig.to_owned(),
        t_ms,
        present_ms,
        clients: atl_clients(&json)?,
    }))
}

/// The rest of a `key value` line. An absent or misnamed line is an error.
fn field<'a>(line: Option<&'a str>, key: &str) -> Result<&'a str> {
    let line = line.with_context(|| format!("the reply ends before `{key}`"))?;
    let rest = line
        .strip_prefix(key)
        .with_context(|| format!("expected `{key}`, got {line:?}"))?;
    Ok(rest.trim())
}

fn no_session(n: &str) -> String {
    match n {
        "0" => "no Hyprland instance under /run/user/<uid>/hypr".to_owned(),
        n => format!("{n} Hyprland instances under /run/user/<uid>/hypr, want 1"),
    }
}

fn probe_script(plan: &Plan) -> String {
    PROBE
        .replace("@BASE@", &remote::sh_quote(&plan.base))
        .replace("@APP@", &plan.app_log)
        .replace("@PRESENT@", run_apk::PRESENT_LINE)
}

/// One `hyprctl -j clients` on the target: its reply is the JSON array.
fn snapshot_script(sig: &str) -> String {
    format!(
        "RT=/run/user/$(id -u)\nHYPRLAND_INSTANCE_SIGNATURE='{sig}' XDG_RUNTIME_DIR=\"$RT\" hyprctl -j clients\n"
    )
}

/// `grim -g` of `geometry` to stdout, on the session's one Wayland socket (`remote::session_script`).
fn grab_script(geometry: &str) -> String {
    format!(
        "RT=/run/user/$(id -u)\n{}WAYLAND_DISPLAY=\"$WL\" XDG_RUNTIME_DIR=\"$RT\" grim -g '{geometry}' -\n",
        remote::session_script("observe")
    )
}

/// One shot: its offset from the first present, whether it is settled (taken, or given up), and the last reason it was not taken.
struct Shot {
    offset_ms: u64,
    done: bool,
    reason: String,
}

#[derive(Serialize)]
struct Record<'a> {
    t_ms: u64,
    present_ms: Option<u64>,
    clients: &'a [Client],
}

/// One run's observation: [`Watch::start`] before the transfer, [`Watch::poll`] from its wait loop, [`Watch::finish`] after it. `run` makes one target call with a time bound (`Target::run_bounded`); the tests pass a fake target.
pub struct Watch<F> {
    run: F,
    plan: Plan,
    dir: PathBuf,
    jsonl: File,
    log: File,
    /// The Hyprland instance, once a tick found one.
    sig: Option<String>,
    /// No further ticks: there is no session to watch.
    finished: bool,
    next_tick: Option<Instant>,
    first_present_ms: Option<u64>,
    shots: Vec<Shot>,
    ticks: u32,
    matched_ticks: u32,
    last_match: Option<Client>,
    last_miss: String,
    errors: u32,
}

impl<F> Watch<F>
where
    F: FnMut(&str, Duration) -> Result<Vec<u8>>,
{
    /// Clears the run's root on the target, so no present line from an earlier run can be read, and opens `observe.jsonl` and `observe.log` in `dir` (the run's log dir). Call before the transfer starts.
    pub fn start(mut run: F, plan: Plan, dir: &Path) -> Result<Self> {
        let clear = format!(
            "cd {} || exit 2\nrm -rf -- \"$PWD/out/{}\"\n",
            remote::sh_quote(&plan.base),
            plan.root
        );
        run(&clear, CALL_LIMIT).context("clearing the run's root on the target")?;
        let shots = plan
            .shots_ms
            .iter()
            .map(|&offset_ms| Shot {
                offset_ms,
                done: false,
                reason: String::new(),
            })
            .collect();
        Ok(Watch {
            run,
            dir: dir.to_path_buf(),
            jsonl: File::create(dir.join("observe.jsonl")).context("creating observe.jsonl")?,
            log: File::create(dir.join("observe.log")).context("creating observe.log")?,
            plan,
            sig: None,
            finished: false,
            next_tick: None,
            first_present_ms: None,
            shots,
            ticks: 0,
            matched_ticks: 0,
            last_match: None,
            last_miss: String::new(),
            errors: 0,
        })
    }

    /// One tick, if one is due. Errors are logged, never returned: the observation does not fail the run.
    pub fn poll(&mut self) {
        self.poll_at(Instant::now());
    }

    fn poll_at(&mut self, now: Instant) {
        if self.finished || self.next_tick.is_some_and(|due| now < due) {
            return;
        }
        self.next_tick = Some(now + TICK);
        let script = probe_script(&self.plan);
        let reply = (self.run)(&script, CALL_LIMIT)
            .and_then(|out| parse_reply(&String::from_utf8_lossy(&out)));
        let outcome = match reply {
            Ok(Reply::NoSession(reason)) if self.sig.is_none() => {
                self.event(format!("observe: skipped: {reason}"));
                self.give_up_shots(&format!("no Hyprland session ({reason})"));
                self.finished = true;
                Ok(())
            }
            Ok(Reply::NoSession(reason)) => Err(format!("the Hyprland session is gone: {reason}")),
            Ok(Reply::Tick(tick)) => self.record(&tick),
            Err(e) => Err(format!("{e:#}")),
        };
        if let Err(why) = outcome {
            self.errors += 1;
            self.event(format!("observe: error: {why}"));
        }
    }

    /// Records one tick and tries the shots that are due.
    fn record(&mut self, tick: &Tick) -> Result<(), String> {
        if self.sig.is_none() {
            self.event(format!("observe: Hyprland instance {}", tick.sig));
            self.sig = Some(tick.sig.clone());
        }
        self.ticks += 1;
        let line = serde_json::to_string(&Record {
            t_ms: tick.t_ms,
            present_ms: tick.present_ms,
            clients: &tick.clients,
        })
        .map_err(|e| format!("encoding a tick: {e}"))?;
        writeln!(self.jsonl, "{line}").map_err(|e| format!("writing observe.jsonl: {e}"))?;
        if self.first_present_ms.is_none()
            && let Some(present) = tick.present_ms
        {
            self.first_present_ms = Some(present);
            self.event(format!(
                "observe: the app's first present at target {present} ms"
            ));
        }
        match matched(&tick.clients) {
            Ok(client) => {
                self.matched_ticks += 1;
                self.last_match = Some(client.clone());
                self.last_miss.clear();
            }
            Err(why) => self.last_miss = why,
        }
        self.shoot_due(tick);
        Ok(())
    }

    /// Each due shot is taken from this tick's window. One that cannot be taken yet waits for the next tick, until its grace ends.
    fn shoot_due(&mut self, tick: &Tick) {
        let Some(present) = self.first_present_ms else {
            return;
        };
        for i in 0..self.shots.len() {
            let offset = self.shots[i].offset_ms;
            let due = present + offset;
            if self.shots[i].done || tick.t_ms < due {
                continue;
            }
            let outcome = match matched(&tick.clients) {
                Ok(client) => self.capture(&tick.sig, client, offset),
                Err(why) => Err(why),
            };
            match outcome {
                Ok(what) => {
                    self.shots[i].done = true;
                    self.event(format!(
                        "observe shot {offset}ms: taken {what}, tick at +{} ms",
                        tick.t_ms - present
                    ));
                }
                Err(why) if tick.t_ms > due + SHOT_GRACE_MS => {
                    self.shots[i].done = true;
                    self.event(format!("observe shot {offset}ms: not taken: {why}"));
                }
                Err(why) => {
                    if self.shots[i].reason != why {
                        self.event(format!("observe shot {offset}ms: waiting: {why}"));
                        self.shots[i].reason = why;
                    }
                }
            }
        }
    }

    /// One shot: `grim -g` of the window's rect, kept only if the rect after the grab is the same. Returns the file name and rect.
    fn capture(&mut self, sig: &str, client: &Client, offset: u64) -> Result<String, String> {
        let geometry = grim_geometry(client).map_err(|e| format!("{e:#}"))?;
        let png =
            (self.run)(&grab_script(&geometry), CALL_LIMIT).map_err(|e| format!("grim: {e:#}"))?;
        if !png.starts_with(PNG_MAGIC) {
            return Err(format!("grim printed {} bytes, not a PNG", png.len()));
        }
        let after = self.snapshot(sig)?;
        let after_client = matched(&after).map_err(|why| format!("after the grab: {why}"))?;
        let after_geometry = grim_geometry(after_client).map_err(|e| format!("{e:#}"))?;
        if after_geometry != geometry {
            return Err(format!(
                "the window moved during the grab ({geometry} -> {after_geometry})"
            ));
        }
        let name = format!("shot-{offset}ms.png");
        std::fs::write(self.dir.join(&name), &png).map_err(|e| format!("writing {name}: {e}"))?;
        Ok(format!("{name}, rect {geometry}"))
    }

    fn snapshot(&mut self, sig: &str) -> Result<Vec<Client>, String> {
        let out =
            (self.run)(&snapshot_script(sig), CALL_LIMIT).map_err(|e| format!("hyprctl: {e:#}"))?;
        atl_clients(&String::from_utf8_lossy(&out)).map_err(|e| format!("{e:#}"))
    }

    /// Gives up every shot not taken yet, with `why` and the last reason it was not taken.
    fn give_up_shots(&mut self, why: &str) {
        for i in 0..self.shots.len() {
            if self.shots[i].done {
                continue;
            }
            self.shots[i].done = true;
            let offset = self.shots[i].offset_ms;
            let last = std::mem::take(&mut self.shots[i].reason);
            let detail = if last.is_empty() {
                why.to_owned()
            } else {
                format!("{why}; last: {last}")
            };
            self.event(format!("observe shot {offset}ms: not taken: {detail}"));
        }
    }

    /// The run has ended: the open shots are given up, and the verdict line names the window's last state, from the last tick that matched it.
    pub fn finish(mut self) {
        if self.finished {
            return;
        }
        let why = if self.first_present_ms.is_some() {
            "the run ended before it was due, or its window never matched"
        } else {
            "the app never presented in this run"
        };
        self.give_up_shots(why);
        let line = match &self.last_match {
            Some(client) => verdict(client, self.matched_ticks, self.ticks),
            None => format!(
                "observe: no atl.* window matched in {} ticks: {}",
                self.ticks,
                if self.last_miss.is_empty() {
                    "no tick answered"
                } else {
                    self.last_miss.as_str()
                }
            ),
        };
        self.event(line);
        if self.errors > 0 {
            self.event(format!("observe: {} target calls failed", self.errors));
        }
    }

    /// One line: printed as it happens, and kept in `observe.log`.
    fn event(&mut self, line: String) {
        println!("{line}");
        if let Err(e) = writeln!(self.log, "{line}") {
            println!("observe: writing observe.log failed: {e}");
        }
    }
}

/// The verdict line: the window's last state, and how many ticks matched it.
pub fn verdict(c: &Client, hits: u32, ticks: u32) -> String {
    format!(
        "observe: {} floating={} mapped={} at={},{} size={}x{} workspace={} (matched {hits}/{ticks} ticks)",
        c.class, c.floating, c.mapped, c.at[0], c.at[1], c.size[0], c.size[1], c.workspace
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    const SIG: &str = "efb5_1794021578_2037488479";
    const PNG_FIXTURE: &[u8] = b"\x89PNG\r\n\x1a\nfixture";
    const HYPR: &str = r#"[
  {"address": "0x1", "mapped": true, "hidden": false, "floating": false, "at": [12, 44], "size": [1776, 1069], "class": "google-chrome", "title": "private", "workspace": {"id": 1, "name": "1"}},
  {"address": "0x2", "mapped": true, "hidden": false, "floating": true, "at": [1000, 200], "size": [720, 960], "class": "atl.com.noodlecake.happyjump", "title": "Happy Jump", "workspace": {"id": 2, "name": "2"}},
  {"address": "0x3", "mapped": true, "hidden": false, "floating": false, "at": [0, 0], "size": [1, 1], "class": "atlas", "workspace": {"id": 3, "name": "3"}}
]"#;
    const TICK_TEXT: &str = r#"session ok efb5_1794021578_2037488479
t 1793887600000
present 1793887559.900
clients-begin
[{"class": "atl.com.noodlecake.happyjump", "mapped": true, "hidden": false, "floating": true, "at": [1000, 200], "size": [720, 960], "workspace": {"id": 2, "name": "2"}}, {"class": "google-chrome", "mapped": true, "hidden": false, "floating": false, "at": [12, 44], "size": [1776, 1069], "workspace": {"id": 1, "name": "1"}}]
clients-end
"#;

    fn client(mapped: bool, hidden: bool, at: [i32; 2], size: [i32; 2]) -> Client {
        Client {
            class: "atl.com.noodlecake.happyjump".to_owned(),
            mapped,
            hidden,
            floating: true,
            at,
            size,
            workspace: "2".to_owned(),
        }
    }

    fn window(at: [i32; 2]) -> Client {
        client(true, false, at, [720, 960])
    }

    fn plan(shots_ms: Vec<u64>) -> Plan {
        Plan {
            base: "atl-next-run/slot-3".to_owned(),
            root: "run-apk/root".to_owned(),
            app_log: "run-apk/root/log/com.noodlecake.happyjump.log".to_owned(),
            shots_ms,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("observe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// `sh` with `script` on stdin and `args`: its output.
    #[cfg(unix)]
    fn sh(args: &[&str], script: &str) -> std::process::Output {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let mut child = Command::new("sh")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(script.as_bytes())
            .expect("script");
        child.wait_with_output().expect("sh output")
    }

    /// The `hyprctl -j clients` JSON for `windows`, in Hyprland's shape.
    fn hypr_array(windows: &[Client]) -> String {
        let items: Vec<String> = windows
            .iter()
            .map(|c| {
                format!(
                    r#"{{"class": "{}", "mapped": {}, "hidden": {}, "floating": {}, "at": [{}, {}], "size": [{}, {}], "workspace": {{"id": 2, "name": "{}"}}}}"#,
                    c.class,
                    c.mapped,
                    c.hidden,
                    c.floating,
                    c.at[0],
                    c.at[1],
                    c.size[0],
                    c.size[1],
                    c.workspace
                )
            })
            .collect();
        format!("[{}]", items.join(", "))
    }

    /// A target in memory: answers the observation's calls from its fields and records the grabs and clears.
    #[derive(Default)]
    struct Fake {
        t_ms: u64,
        present_ms: Option<u64>,
        windows: Vec<Client>,
        /// Replaces `windows` at the next grab: a window that moves mid-capture.
        moves_at_grab: Option<Vec<Client>>,
        has_session: bool,
        grabs: Vec<String>,
        cleared: Vec<String>,
    }

    fn target(state: &Rc<RefCell<Fake>>) -> impl FnMut(&str, Duration) -> Result<Vec<u8>> {
        let state = Rc::clone(state);
        move |script: &str, _limit: Duration| -> Result<Vec<u8>> {
            let mut f = state.borrow_mut();
            if script.contains("rm -rf") {
                f.cleared.push(script.to_owned());
                return Ok(Vec::new());
            }
            if script.contains("clients-begin") {
                if !f.has_session {
                    return Ok(b"session none 0\n".to_vec());
                }
                let stamp = f
                    .present_ms
                    .map(|p| format!("{}.{:03}", p / 1000, p % 1000))
                    .unwrap_or_default();
                return Ok(format!(
                    "session ok {SIG}\nt {}\npresent {stamp}\nclients-begin\n{}\nclients-end\n",
                    f.t_ms,
                    hypr_array(&f.windows)
                )
                .into_bytes());
            }
            if let Some(geometry) = script
                .split("grim -g '")
                .nth(1)
                .and_then(|rest| rest.split('\'').next())
            {
                f.grabs.push(geometry.to_owned());
                if let Some(moved) = f.moves_at_grab.take() {
                    f.windows = moved;
                }
                return Ok(PNG_FIXTURE.to_vec());
            }
            if script.contains("hyprctl -j clients") {
                return Ok(hypr_array(&f.windows).into_bytes());
            }
            Err(anyhow::anyhow!(
                "the fake target has no answer for: {script}"
            ))
        }
    }

    /// `ticks` one-second ticks from target time `from_ms`; `step` sets the fake's state before each.
    fn run_ticks(
        watch: &mut Watch<impl FnMut(&str, Duration) -> Result<Vec<u8>>>,
        state: &Rc<RefCell<Fake>>,
        from_ms: u64,
        ticks: u32,
        mut step: impl FnMut(&mut Fake),
    ) {
        let start = Instant::now();
        for i in 0..ticks {
            {
                let mut f = state.borrow_mut();
                f.t_ms = from_ms + 1000 * u64::from(i);
                step(&mut f);
            }
            watch.poll_at(start + TICK * i);
        }
    }

    #[test]
    fn the_filter_keeps_only_atl_dot_classes() {
        let got = atl_clients(HYPR).expect("json");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].class, "atl.com.noodlecake.happyjump");
        assert!(got[0].floating && got[0].mapped && !got[0].hidden);
        assert_eq!((got[0].at, got[0].size), ([1000, 200], [720, 960]));
        assert_eq!(got[0].workspace, "2");
    }

    #[test]
    fn a_shot_needs_exactly_one_mapped_visible_window() {
        assert!(matched(&[]).is_err());
        assert!(matched(&[client(false, false, [0, 0], [720, 960])]).is_err());
        assert!(matched(&[client(true, true, [0, 0], [720, 960])]).is_err());
        let two = [window([0, 0]), window([5, 5])];
        assert!(matched(&two).unwrap_err().contains("2 mapped"));
        let one_hidden = [client(true, true, [0, 0], [1, 1]), window([7, 7])];
        assert_eq!(matched(&one_hidden).expect("one").at, [7, 7]);
    }

    #[test]
    fn the_grim_argument_is_the_layout_rect() {
        let mut c = window([1000, 200]);
        assert_eq!(grim_geometry(&c).expect("rect"), "1000,200 720x960");
        c.at = [-1280, 0];
        assert_eq!(grim_geometry(&c).expect("rect"), "-1280,0 720x960");
        c.size = [0, 960];
        assert!(grim_geometry(&c).is_err());
    }

    #[test]
    fn the_verdict_names_the_window_state() {
        assert_eq!(
            verdict(&window([1000, 200]), 58, 60),
            "observe: atl.com.noodlecake.happyjump floating=true mapped=true at=1000,200 size=720x960 workspace=2 (matched 58/60 ticks)"
        );
    }

    #[test]
    fn a_tick_reply_is_read_field_by_field() {
        let Reply::Tick(t) = parse_reply(TICK_TEXT).expect("reply") else {
            panic!("not a tick");
        };
        assert_eq!(t.sig, SIG);
        assert_eq!(t.t_ms, 1_793_887_600_000);
        assert_eq!(t.present_ms, Some(1_793_887_559_900));
        assert_eq!(t.clients.len(), 1);
    }

    #[test]
    fn a_missing_session_is_a_reason_not_an_error() {
        assert_eq!(
            parse_reply("session none 0\n").expect("reply"),
            Reply::NoSession("no Hyprland instance under /run/user/<uid>/hypr".to_owned())
        );
        assert_eq!(
            parse_reply("session none 2\n").expect("reply"),
            Reply::NoSession("2 Hyprland instances under /run/user/<uid>/hypr, want 1".to_owned())
        );
        assert_eq!(
            parse_reply("session no-hyprctl\n").expect("reply"),
            Reply::NoSession("hyprctl is not on the target's PATH".to_owned())
        );
    }

    #[test]
    fn a_truncated_or_odd_reply_is_an_error() {
        assert!(parse_reply("session ok a_1\nt 5\npresent \nclients-begin\n[]\n").is_err());
        assert!(
            parse_reply("session ok ../etc\nt 5\npresent \nclients-begin\n[]\nclients-end\n")
                .is_err()
        );
        assert!(parse_reply("hello\n").is_err());
        let Reply::Tick(t) =
            parse_reply("session ok a_1\nt 5\npresent \nclients-begin\n[]\nclients-end\n")
                .expect("reply")
        else {
            panic!("not a tick");
        };
        assert_eq!(t.present_ms, None);
    }

    #[cfg(unix)]
    #[test]
    fn the_target_scripts_parse_as_sh() {
        for script in [
            probe_script(&plan(vec![])),
            snapshot_script(SIG),
            grab_script("1000,200 720x960"),
        ] {
            let out = sh(&["-n"], &script);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// Where no Hyprland instance runs, the real probe answers `session none 0` (a host with one runs the check on its own session instead).
    #[cfg(unix)]
    #[test]
    fn the_probe_reports_no_session_where_there_is_no_hyprland() {
        let uid = String::from_utf8_lossy(
            &std::process::Command::new("id")
                .arg("-u")
                .output()
                .expect("id")
                .stdout,
        )
        .trim()
        .to_owned();
        if Path::new("/run/user").join(&uid).join("hypr").exists() {
            return;
        }
        let mut p = plan(vec![]);
        p.base = std::env::temp_dir().display().to_string();
        let out = sh(&[], &probe_script(&p));
        let reply = parse_reply(&String::from_utf8_lossy(&out.stdout)).expect("reply");
        assert!(matches!(reply, Reply::NoSession(_)), "{reply:?}");
    }

    #[test]
    fn a_shot_waits_for_its_due_time_and_a_matched_window() {
        let dir = scratch("due");
        let state = Rc::new(RefCell::new(Fake {
            has_session: true,
            ..Fake::default()
        }));
        let mut watch = Watch::start(target(&state), plan(vec![5000]), &dir).expect("start");
        run_ticks(&mut watch, &state, 1_000_000, 12, |f| {
            if f.t_ms >= 1_002_000 {
                f.present_ms = Some(1_002_000);
            }
            if f.t_ms >= 1_003_000 {
                f.windows = vec![window([1000, 200])];
            }
        });
        watch.finish();
        let f = state.borrow();
        assert_eq!(f.cleared.len(), 1);
        assert!(
            f.cleared[0].contains("out/run-apk/root"),
            "{}",
            f.cleared[0]
        );
        // due at 1_002_000 + 5000: the first tick then with a window, and only one grab
        assert_eq!(f.grabs, vec!["1000,200 720x960".to_owned()]);
        assert_eq!(
            std::fs::read(dir.join("shot-5000ms.png")).expect("shot"),
            PNG_FIXTURE
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("observe.jsonl"))
                .expect("jsonl")
                .lines()
                .count(),
            12
        );
    }

    #[test]
    fn a_shot_is_not_taken_without_a_matched_window() {
        let dir = scratch("nowin");
        let state = Rc::new(RefCell::new(Fake {
            has_session: true,
            ..Fake::default()
        }));
        let mut watch = Watch::start(target(&state), plan(vec![5000]), &dir).expect("start");
        run_ticks(&mut watch, &state, 1_000_000, 20, |f| {
            if f.t_ms >= 1_002_000 {
                f.present_ms = Some(1_002_000);
            }
            f.windows = vec![Client {
                hidden: true,
                ..window([1000, 200])
            }];
        });
        watch.finish();
        assert!(
            state.borrow().grabs.is_empty(),
            "a grab without a matched window"
        );
        assert!(!dir.join("shot-5000ms.png").exists());
        let log = std::fs::read_to_string(dir.join("observe.log")).expect("log");
        assert!(log.contains("observe shot 5000ms: not taken"), "{log}");
    }

    #[test]
    fn a_window_that_moves_during_the_grab_is_not_kept() {
        let dir = scratch("moved");
        let state = Rc::new(RefCell::new(Fake {
            has_session: true,
            windows: vec![window([1000, 200])],
            moves_at_grab: Some(vec![window([1100, 300])]),
            ..Fake::default()
        }));
        let mut watch = Watch::start(target(&state), plan(vec![5000]), &dir).expect("start");
        run_ticks(&mut watch, &state, 1_000_000, 12, |f| {
            if f.t_ms >= 1_002_000 {
                f.present_ms = Some(1_002_000);
            }
        });
        watch.finish();
        assert_eq!(
            state.borrow().grabs,
            vec!["1000,200 720x960".to_owned(), "1100,300 720x960".to_owned()]
        );
        let log = std::fs::read_to_string(dir.join("observe.log")).expect("log");
        assert!(
            log.contains("moved during the grab (1000,200 720x960 -> 1100,300 720x960)"),
            "{log}"
        );
        assert!(dir.join("shot-5000ms.png").is_file());
    }

    #[test]
    fn no_hyprland_session_skips_the_observation_with_a_reason() {
        let dir = scratch("nosession");
        let state = Rc::new(RefCell::new(Fake::default()));
        let mut watch = Watch::start(target(&state), plan(vec![5000]), &dir).expect("start");
        run_ticks(&mut watch, &state, 1_000_000, 5, |_| {});
        watch.finish();
        assert!(state.borrow().grabs.is_empty());
        let log = std::fs::read_to_string(dir.join("observe.log")).expect("log");
        assert!(
            log.contains("observe: skipped: no Hyprland instance"),
            "{log}"
        );
        assert!(log.contains("observe shot 5000ms: not taken"), "{log}");
    }
}
