// SPDX-License-Identifier: MIT
//! `xtask test binder`: AOSP's servicemanager and `service` command, built by the converter on libbinder's driver seam (patches/aosp/frameworks/native 0003), talking through binderd.
//!
//! The harness stands in for the broker: it starts `atl-binderd` (admin connection on its stdin), and per process makes a SEQPACKET socketpair, passes one end as `ATL_BINDER_FD`, registers the other with binderd tagged {uid, host pid}.
//! servicemanager: uid 1000 (AID_SYSTEM), the one process allowed to become context manager; run under bubblewrap with the test layout's /system, /vendor, /odm (empty VINTF manifests) over the host's root.
//! `service`: uid 2000 (AID_SHELL). Gate: `service list` lists `manager` (servicemanager registers itself, main.cpp), `service check manager` finds it, `service check <absent>` does not; binderd's counters show the driver paths that must fire on this workload: the context manager's TXN_SECURITY_CTX flag (ignored) and libbinder's oneway-spam ioctl (unsupported).
//! Then stage `services` (services.rs): the M2 Rust services registered with that servicemanager and called through binderd. Needs `xtask native` to have built crate atl-services.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding servicemanager and service (default <workspace>/out/native/<host>, as `xtask native`).
    #[arg(long)]
    out: Option<PathBuf>,
}

/// Android uids (system/core/libcutils/include/private/android_filesystem_config.h): AID_SYSTEM, AID_SHELL.
const AID_SYSTEM: u32 = 1000;
const AID_SHELL: u32 = 2000;
/// libbinder's driver-fd variable (atl-binder-driver `CONTROL_ENV`).
const CONTROL_ENV: &str = "ATL_BINDER_FD";
/// The name servicemanager registers itself under (cmds/servicemanager/main.cpp `addService("manager", ...)`).
const MANAGER: &str = "manager";
/// A name nothing registers, shaped as an AIDL HAL instance (`<package>.<Interface>/<instance>`) so servicemanager's miss path consults the VINTF manifests (ServiceManager.cpp `isVintfDeclared` via `NativeName::fill` / AIDL instance names): without them it logs `NULL VINTF MANIFEST`.
const ABSENT: &str = "android.hardware.atltest.IAbsent/default";
/// How long servicemanager may take to answer its first `service check` (a cold start reads VINTF and starts its Looper: well under a second measured; 10 s leaves room for a loaded machine).
const STARTUP: Duration = Duration::from_secs(10);
/// binderd counter lines this workload must produce (atl-binderd main.rs prints them; any nonzero count): servicemanager's nodes carry TXN_SECURITY_CTX (`becomeContextManager`, and `setRequestingSid` on its services), and libbinder sends BINDER_ENABLE_ONEWAY_SPAM_DETECTION at open (ProcessState.cpp `open_driver`). Measured 2026-11-02: 3 and 4.
const MUST_FIRE: &[&str] = &[
    "binderd: ignored TxnSecurityCtx ",
    "binderd: fault UnsupportedIoctl ",
];

/// An empty VINTF manifest of `kind` (device / framework): libvintf's schema (system/libvintf/parse_xml.cpp `manifest` element, version attribute required).
fn manifest(kind: &str) -> String {
    format!("<manifest version=\"1.0\" type=\"{kind}\">\n</manifest>\n")
}

/// The test /system, /vendor, /odm under `root`: an empty framework manifest (/system/etc/vintf/manifest.xml) and device manifests (/vendor, /odm), libvintf's default paths (system/libvintf/include/vintf/constants.h).
fn layout(root: &Path) -> Result<()> {
    for (dir, kind) in [
        ("system", "framework"),
        ("vendor", "device"),
        ("odm", "device"),
    ] {
        let d = root.join(dir).join("etc/vintf");
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("manifest.xml"), manifest(kind))?;
    }
    Ok(())
}

/// bubblewrap arguments: a tmpfs root holding every entry of the host's `/` (dirs bound, symlinks recreated), plus `root`'s system, vendor, odm.
fn sandbox_args(root: &Path) -> Result<Vec<String>> {
    let mut a: Vec<String> = vec!["--tmpfs".into(), "/".into()];
    let ours = ["system", "vendor", "odm"];
    for e in std::fs::read_dir("/")? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if ours.contains(&name.as_str()) {
            continue;
        }
        let path = format!("/{name}");
        let ft = e.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::read_link(&path)?;
            a.extend(["--symlink".into(), target.display().to_string(), path]);
        } else if ft.is_dir() {
            let kind = match name.as_str() {
                "dev" => "--dev-bind",
                "proc" => "--proc",
                _ => "--bind",
            };
            if kind == "--proc" {
                a.extend([kind.into(), path]);
            } else {
                a.extend([kind.into(), path.clone(), path]);
            }
        }
    }
    for d in ours {
        a.extend([
            "--ro-bind".into(),
            root.join(d).display().to_string(),
            format!("/{d}"),
        ]);
    }
    Ok(a)
}

/// The broker's side of binderd: its admin connection. Dropping it closes the connection, which ends binderd.
pub(crate) struct Harness {
    admin: OwnedFd,
}

/// atl-binderd, built into `out`'s cargo target dir as `xtask native` builds crates, started with its admin connection on stdin and its stderr (counters) in `log`.
pub(crate) fn start_binderd(root: &Path, out: &Path, log: &Path) -> Result<(Child, Harness)> {
    let status = crate::cargo_command()
        .args([
            "build",
            "--release",
            "-p",
            "atl-binderd",
            "--bin",
            "atl-binderd",
            "--target-dir",
        ])
        .arg(out.join("cargo"))
        .current_dir(root)
        .status()?;
    ensure!(status.success(), "building atl-binderd failed");
    let (admin, theirs) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let binderd = Command::new(out.join("cargo/release/atl-binderd"))
        .stdin(Stdio::from(theirs))
        .stderr(std::fs::File::create(log)?)
        .spawn()
        .context("starting atl-binderd")?;
    Ok((binderd, Harness { admin }))
}

impl Harness {
    /// Spawns `cmd` as a binder process tagged `uid` (context manager if `cm`), its control connection inherited.
    pub(crate) fn spawn(&self, mut cmd: Command, uid: u32, cm: bool) -> Result<Child> {
        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        // inherited by this child only: cleared just before the spawn, closed right after it
        fcntl_setfd(&theirs, FdFlags::empty())?;
        cmd.env(CONTROL_ENV, theirs.as_raw_fd().to_string());
        let child = cmd.spawn().context("spawning a binder process")?;
        drop(theirs);
        let pid = i32::try_from(child.id())?;
        atl_binderd::server::register(self.admin.as_fd(), ours.as_fd(), uid, pid, cm)
            .context("registering with binderd")?;
        Ok(child)
    }

    /// `service <args>` as the shell uid: (success, stdout + stderr), output through `log` (a file: a pipe would hang on any grandchild holding it). Killed after `RUN_LIMIT`.
    fn service(
        &self,
        service: &Path,
        env: &[(String, String)],
        args: &[&str],
        log: &Path,
    ) -> Result<(bool, String)> {
        let file = std::fs::File::create(log)?;
        let mut cmd = Command::new(service);
        cmd.args(args)
            .envs(env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file);
        let mut child = self.spawn(cmd, AID_SHELL, false)?;
        let status = wait_limited(&mut child, RUN_LIMIT)?;
        let text = std::fs::read_to_string(log)?;
        Ok(match status {
            Some(s) => (s.success(), text),
            None => (false, format!("{text}(killed after {RUN_LIMIT:?})")),
        })
    }
}

/// How long one `service` run may take: a lookup is one binder round trip (well under 100 ms); a hang means a lost reply.
const RUN_LIMIT: Duration = Duration::from_secs(5);

/// `child`'s exit status, or None after killing it at `limit`.
fn wait_limited(child: &mut Child, limit: Duration) -> Result<Option<std::process::ExitStatus>> {
    let start = Instant::now();
    loop {
        if let Some(s) = child.try_wait()? {
            return Ok(Some(s));
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            child.wait()?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The gate's verdict over the three `service` runs and binderd's counter output.
fn verdict(
    list: &(bool, String),
    found: &(bool, String),
    absent: &(bool, String),
    counters: &str,
) -> Vec<String> {
    let mut fails = Vec::new();
    if !list.0
        || !list
            .1
            .contains(&format!("{MANAGER}: [android.os.IServiceManager]"))
    {
        fails.push(format!(
            "service list does not list {MANAGER}: {}",
            list.1.trim()
        ));
    }
    if !found.0 || !found.1.contains(&format!("Service {MANAGER}: found")) {
        fails.push(format!("service check {MANAGER}: {}", found.1.trim()));
    }
    if !absent.1.contains(&format!("Service {ABSENT}: not found")) {
        fails.push(format!("service check {ABSENT}: {}", absent.1.trim()));
    }
    for want in MUST_FIRE {
        if !counters.lines().any(|l| l.starts_with(want)) {
            fails.push(format!("binderd counter missing: {want}"));
        }
    }
    fails
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let layout_ = Layout::of(root)?;
    let out = match &args.out {
        Some(dir) => std::path::absolute(dir)?,
        None => layout_.workspace().join("out/native").join(&args.host),
    };
    let device = out.join("device");
    let servicemanager = device.join("servicemanager/servicemanager");
    let service = device.join("service/service");
    for b in [&servicemanager, &service] {
        ensure!(
            b.is_file(),
            "{} missing: cargo xtask native --host {} --out {} servicemanager service",
            b.display(),
            args.host,
            out.display()
        );
    }
    let test_root = out.join("binder-test");
    if test_root.exists() {
        std::fs::remove_dir_all(&test_root)?;
    }
    layout(&test_root)?;
    // every library the converter linked, and the provided crates' (libatl_binder_driver), as native-load stages them
    let mut lib_dirs: Vec<String> = Vec::new();
    for dir in [device.clone(), out.join("crates")] {
        for e in std::fs::read_dir(&dir)? {
            let p = e?.path();
            let p = if dir == device { p } else { p.join("lib") };
            if p.is_dir() {
                lib_dirs.push(p.display().to_string());
            }
        }
    }
    let env = vec![("LD_LIBRARY_PATH".to_owned(), lib_dirs.join(":"))];

    let (mut binderd, h) = start_binderd(root, &out, &test_root.join("binderd.log"))?;

    // bwrap stays the sandbox's pid 1 (--unshare-pid) and takes servicemanager with it when killed (--die-with-parent)
    let sm_log_path = test_root.join("servicemanager.log");
    let sm_log_file = std::fs::File::create(&sm_log_path)?;
    let mut sm = Command::new("bwrap");
    sm.args(["--unshare-pid", "--die-with-parent"])
        .args(sandbox_args(&test_root)?)
        .arg("--")
        .arg(&servicemanager)
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(sm_log_file.try_clone()?)
        .stderr(sm_log_file);
    let mut sm = h.spawn(sm, AID_SYSTEM, true)?;

    // servicemanager answers once its Looper runs
    let start = Instant::now();
    let log = |n: &str| test_root.join(format!("service-{n}.log"));
    let found = loop {
        let r = h.service(&service, &env, &["check", MANAGER], &log("check"))?;
        if r.0 && r.1.contains("found") && !r.1.contains("not found") || start.elapsed() > STARTUP {
            break r;
        }
        if let Some(status) = sm.try_wait()? {
            break (false, format!("servicemanager exited: {status}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let list = h.service(&service, &env, &["list"], &log("list"))?;
    let absent = h.service(&service, &env, &["check", ABSENT], &log("absent"))?;
    let services_fails = crate::services::stage(root, &out, &h, &env, &service, &test_root)?;

    let _ = sm.kill();
    sm.wait()?;
    drop(h);
    let bstatus = wait_limited(&mut binderd, RUN_LIMIT)?;
    let counters = std::fs::read_to_string(test_root.join("binderd.log"))?;
    let sm_log = std::fs::read_to_string(&sm_log_path)?;
    print!("{counters}");
    if sm_log.contains("NULL VINTF MANIFEST") {
        println!("binder: WARN servicemanager logs NULL VINTF MANIFEST");
    }
    let mut fails = verdict(&list, &found, &absent, &counters);
    fails.extend(services_fails);
    match bstatus {
        Some(s) if s.success() => {}
        Some(s) => fails.push(format!("atl-binderd exited {s}")),
        None => fails.push("atl-binderd did not exit when its admin connection closed".into()),
    }
    for f in &fails {
        println!("binder: FAIL {f}");
    }
    if fails.is_empty() {
        println!(
            "binder: pass (service list / check manager / check {ABSENT} through binderd to AOSP servicemanager)"
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("servicemanager log:\n{sm_log}");
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each check fires on its own: a missing listing, a failed lookup, an absent name reported found, a counter that did not fire.
    #[test]
    fn verdicts() {
        let list = (
            true,
            format!("Found 1 services:\n0\t{MANAGER}: [android.os.IServiceManager]\n"),
        );
        let found = (true, format!("Service {MANAGER}: found\n"));
        let absent = (true, format!("Service {ABSENT}: not found\n"));
        let counters = "binderd: ignored TxnSecurityCtx 1\nbinderd: fault UnsupportedIoctl 2\n";
        assert!(verdict(&list, &found, &absent, counters).is_empty());
        assert_eq!(
            verdict(&(true, String::new()), &found, &absent, counters).len(),
            1
        );
        assert_eq!(
            verdict(&list, &(false, String::new()), &absent, counters).len(),
            1
        );
        assert_eq!(
            verdict(
                &list,
                &found,
                &(true, format!("Service {ABSENT}: found")),
                counters
            )
            .len(),
            1
        );
        assert_eq!(
            verdict(
                &list,
                &found,
                &absent,
                "binderd: ignored TxnSecurityCtx 1\n"
            )
            .len(),
            1
        );
    }

    #[test]
    fn manifests_and_layout() {
        let dir = std::env::temp_dir().join(format!("atl-binder-layout-{}", std::process::id()));
        layout(&dir).unwrap();
        let s = std::fs::read_to_string(dir.join("vendor/etc/vintf/manifest.xml")).unwrap();
        assert!(roxmltree::Document::parse(&s).is_ok());
        assert!(dir.join("system/etc/vintf/manifest.xml").is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// libbinder's files holding the driver I/O (patches/aosp/frameworks/native 0003), relative to `src/`.
const SEAM_FILES: &[&str] = &[
    "aosp/frameworks/native/libs/binder/ProcessState.cpp",
    "aosp/frameworks/native/libs/binder/IPCThreadState.cpp",
];

/// Lines of `text` that do driver I/O past the seam: a raw `ioctl(` / `mmap(` / `munmap(` anywhere (every one in these files is on the driver), or a raw `open(` / `close(` on a line naming the driver (`mDriverFD`, `driver`). `binder_driver_*` calls and other identifiers ending in these names (`::close`, `fclose`) are not raw.
/// Limit: a driver fd copied into another variable first and closed on a later line is not seen; review catches it, the gate guards the patched one-line sites.
fn seam_violations(text: &str) -> Vec<(usize, String)> {
    let raw = |line: &str, call: &str| {
        line.match_indices(call).any(|(i, _)| {
            line[..i].chars().next_back().is_none_or(|c| {
                !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.' || c == '>')
            })
        })
    };
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            // code only: without the line comment and the text of string literals (log messages name ioctl())
            let mut code = String::new();
            let mut quoted = false;
            let mut prev = ' ';
            for c in l.split("//").next().unwrap_or_default().chars() {
                if c == '"' && prev != '\\' {
                    quoted = !quoted;
                } else if !quoted {
                    code.push(c);
                }
                prev = c;
            }
            let code = code.as_str();
            ["ioctl(", "mmap(", "munmap("].iter().any(|c| raw(code, c))
                || ((code.contains("mDriverFD") || code.contains("driver"))
                    && ["open(", "close("].iter().any(|c| raw(code, c)))
        })
        .map(|(i, l)| (i + 1, l.trim().to_owned()))
        .collect()
}

/// `xtask check` step `seam`: no driver I/O in libbinder bypasses `BinderDriver.h`, in the tree this checkout builds (task copy or `src/`).
pub fn seam_check(root: &Path) -> Result<()> {
    let layout = Layout::of(root)?;
    let mut bad = Vec::new();
    for f in SEAM_FILES {
        let path = layout.locate(Path::new(f)).dir;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("{}: cargo xtask fetch --execute", path.display()))?;
        for (n, l) in seam_violations(&text) {
            bad.push(format!("{}:{n}: {l}", path.display()));
        }
    }
    ensure!(
        bad.is_empty(),
        "driver I/O past BinderDriver.h:\n{}",
        bad.join("\n")
    );
    eprintln!(
        "seam: ok ({} files, every driver call through binder_driver_*)",
        SEAM_FILES.len()
    );
    Ok(())
}

#[cfg(test)]
mod seam_tests {
    use super::seam_violations;

    #[test]
    fn raw_driver_calls_fire() {
        let ok = "x = binder_driver_ioctl(mDriverFD, BINDER_WRITE_READ, &bwr);\nbinder_driver_close(mDriverFD);\n    close(fd); // feature file\n";
        assert!(seam_violations(ok).is_empty());
        let bad = "if (ioctl(mDriverFD, BINDER_WRITE_READ, &bwr) >= 0)\n  close(gProcess->mDriverFD);\n p = mmap(nullptr, 1, 0, 0, fd, 0);\n";
        assert_eq!(
            seam_violations(bad).iter().map(|v| v.0).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
