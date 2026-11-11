// SPDX-License-Identifier: MIT
//! atl-launch: how a user starts an ATL app on the GPU they chose. ATL selects no GPU and reads no GPU env var: the desktop decides.
//! - `default`: the command runs unchanged, on the desktop's default GPU.
//! - `discrete`: from a terminal the command is re-executed through `switcherooctl launch` (switcheroo-control), which applies the desktop's own offload env for the non-default GPU;
//!   from a desktop launcher the generated `.desktop` entry carries `PrefersNonDefaultGPU=true` (Desktop Entry spec 1.4) and the launcher applies it. No switcherooctl -> [`LaunchError::NoSwitcheroo`], never a silent fallback.
//! - the per-app preference lives in one TOML file ([`Settings`]); the `.desktop` writer reads it.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// switcheroo-control's CLI. `launch [--gpu N] CMD...` runs CMD with the env switcheroo publishes for GPU N, default the first non-default GPU [recall: switcherooctl(1); source fetch was blocked].
pub const SWITCHEROOCTL: &str = "switcherooctl";

/// The GPU an app runs on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Gpu {
    /// The desktop's default GPU: nothing changes.
    #[default]
    Default,
    /// The desktop's non-default (discrete) GPU, chosen by the desktop's own mechanism.
    Discrete,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(
        "--gpu=discrete needs `switcherooctl` (switcheroo-control) on PATH, which applies the desktop's offload env; it is not installed. Install switcheroo-control, or run the command with the GPU driver's own offload variables set (NVIDIA: __NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia __VK_LAYER_NV_optimus=NVIDIA_only; Mesa: DRI_PRIME=1) and --gpu=default"
    )]
    NoSwitcheroo,
    #[error("no command given")]
    NoCommand,
    #[error("cannot execute {program:?}: {source}")]
    Exec {
        program: OsString,
        source: std::io::Error,
    },
}

/// The argv that runs `cmd` on `gpu`.
pub fn argv(gpu: Gpu, cmd: &[OsString]) -> Result<Vec<OsString>, LaunchError> {
    if cmd.is_empty() {
        return Err(LaunchError::NoCommand);
    }
    Ok(match gpu {
        Gpu::Default => cmd.to_vec(),
        Gpu::Discrete => [OsString::from(SWITCHEROOCTL), OsString::from("launch")]
            .into_iter()
            .chain(cmd.iter().cloned())
            .collect(),
    })
}

/// Replaces the process with `argv`; returns only on failure. A missing switcherooctl is the named error, any other failure names the program.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "the user's launcher runs before any sandbox: it execs atld (the broker's host) or switcherooctl, never an app process"
)]
pub fn exec(argv: &[OsString]) -> LaunchError {
    use std::os::unix::process::CommandExt;
    let Some((program, args)) = argv.split_first() else {
        return LaunchError::NoCommand;
    };
    let source = std::process::Command::new(program).args(args).exec();
    if program == SWITCHEROOCTL && source.kind() == std::io::ErrorKind::NotFound {
        return LaunchError::NoSwitcheroo;
    }
    LaunchError::Exec {
        program: program.clone(),
        source,
    }
}

/// Per-app preferences, keyed by package name: `[apps."<package>"] gpu = "discrete"`. A package not listed runs on `default`.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub apps: BTreeMap<String, AppSettings>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default)]
    pub gpu: Gpu,
}

impl Settings {
    /// Reads `path`; a missing file is the empty settings.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, toml::to_string(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn gpu(&self, package: &str) -> Gpu {
        self.apps.get(package).map(|a| a.gpu).unwrap_or_default()
    }

    pub fn set_gpu(&mut self, package: &str, gpu: Gpu) {
        self.apps.entry(package.to_owned()).or_default().gpu = gpu;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("package name {0:?} is not an Android package name ([A-Za-z0-9_.])")]
    Package(String),
    #[error("{field} contains a control character: {value:?}")]
    Control { field: &'static str, value: String },
}

/// What a generated entry describes.
pub struct DesktopApp<'a> {
    pub package: &'a str,
    pub name: &'a str,
    /// The command the launcher runs (the ATL invocation for this app), one element per argument.
    pub exec: &'a [String],
    pub gpu: Gpu,
}

/// Desktop file ID: `atl.<package>.desktop` (spec "Desktop File ID": reverse-DNS, no clash with a native app's own ID).
pub fn desktop_file_name(package: &str) -> Result<String, DesktopError> {
    check_package(package)?;
    Ok(format!("atl.{package}.desktop"))
}

fn check_package(package: &str) -> Result<(), DesktopError> {
    if package.is_empty()
        || !package
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    {
        return Err(DesktopError::Package(package.to_owned()));
    }
    Ok(())
}

/// The entry's text. `Exec` runs the command with `--gpu=default` semantics (as given): for a discrete app the desktop applies the offload env itself on `PrefersNonDefaultGPU=true`, so ATL must not re-route it again.
pub fn desktop_entry(app: &DesktopApp) -> Result<String, DesktopError> {
    check_package(app.package)?;
    let name = string_value("Name", app.name)?;
    let exec = app
        .exec
        .iter()
        .map(|a| exec_arg(a))
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    let mut out = String::from("[Desktop Entry]\nType=Application\nVersion=1.4\n");
    out += &format!("Name={name}\nExec={}\n", string_value("Exec", &exec)?);
    if app.gpu == Gpu::Discrete {
        out += "PrefersNonDefaultGPU=true\n";
    }
    Ok(out)
}

/// Spec "Possible value types", string: `\s \n \t \r \\` escapes; other control characters are not representable.
fn string_value(field: &'static str, value: &str) -> Result<String, DesktopError> {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out += "\\\\",
            '\n' => out += "\\n",
            '\t' => out += "\\t",
            '\r' => out += "\\r",
            c if c.is_control() => {
                return Err(DesktopError::Control {
                    field,
                    value: value.to_owned(),
                });
            }
            c => out.push(c),
        }
    }
    if out.starts_with(' ') {
        out.replace_range(0..1, "\\s");
    }
    Ok(out)
}

/// Spec "The Exec key": an argument with a reserved character is double-quoted, `" ` $ \` escaped by backslash inside; `%` is a field code prefix, so a literal one is `%%`.
fn exec_arg(arg: &str) -> Result<String, DesktopError> {
    const RESERVED: &[char] = &[
        ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(',
        ')', '`',
    ];
    if arg
        .chars()
        .any(|c| c.is_control() && c != '\t' && c != '\n')
    {
        return Err(DesktopError::Control {
            field: "Exec",
            value: arg.to_owned(),
        });
    }
    let arg = arg.replace('%', "%%");
    if !arg.is_empty() && !arg.contains(RESERVED) {
        return Ok(arg);
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn default_runs_the_command_unchanged() {
        assert_eq!(
            argv(Gpu::Default, &os(&["atld", "--root", "r"])).unwrap(),
            os(&["atld", "--root", "r"])
        );
    }

    #[test]
    fn discrete_goes_through_switcherooctl_launch() {
        assert_eq!(
            argv(Gpu::Discrete, &os(&["atld", "--root", "r"])).unwrap(),
            os(&["switcherooctl", "launch", "atld", "--root", "r"])
        );
    }

    #[test]
    fn empty_command_is_refused() {
        assert!(matches!(
            argv(Gpu::Discrete, &[]),
            Err(LaunchError::NoCommand)
        ));
    }

    // missing switcherooctl and the exec argv with a stub on PATH: tests/cli.rs (needs its own PATH, so a child process)

    #[test]
    fn other_exec_failures_name_the_program() {
        let e = exec(&os(&["/nonexistent/atld"]));
        assert!(matches!(e, LaunchError::Exec { .. }), "{e}");
    }

    #[test]
    fn settings_round_trip_and_default() {
        let dir = std::env::temp_dir().join(format!("atl-launch-settings-{}", std::process::id()));
        let path = dir.join("apps.toml");
        let mut s = Settings::load(&path).unwrap();
        assert_eq!(s.gpu("com.example.game"), Gpu::Default);
        s.set_gpu("com.example.game", Gpu::Discrete);
        s.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // the stored form is the documented one: [apps."<package>"] gpu = "discrete"
        assert!(
            text.contains("[apps.\"com.example.game\"]") && text.contains("gpu = \"discrete\""),
            "{text}"
        );
        assert_eq!(
            Settings::load(&path).unwrap().gpu("com.example.game"),
            Gpu::Discrete
        );
        assert_eq!(
            Settings::load(&path).unwrap().gpu("com.example.other"),
            Gpu::Default
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn app(gpu: Gpu) -> DesktopApp<'static> {
        const EXEC: &[String] = &[];
        DesktopApp {
            package: "com.example.game",
            name: "Example Game",
            exec: EXEC,
            gpu,
        }
    }

    #[test]
    fn discrete_entry_matches_golden() {
        let exec = [
            "atl-launch".to_owned(),
            "run".to_owned(),
            "--".to_owned(),
            "/opt/atl/run game".to_owned(),
            "100%".to_owned(),
        ];
        let entry = desktop_entry(&DesktopApp {
            exec: &exec,
            ..app(Gpu::Discrete)
        })
        .unwrap();
        // the golden's first line is its SPDX comment (repo rule), not generator output
        let golden = include_str!("../testdata/discrete.desktop");
        let golden = golden.split_once('\n').map(|(_, rest)| rest).unwrap();
        assert_eq!(entry, golden);
    }

    #[test]
    fn key_present_iff_discrete() {
        let exec = ["atl-launch".to_owned()];
        let d = desktop_entry(&DesktopApp {
            exec: &exec,
            ..app(Gpu::Discrete)
        })
        .unwrap();
        let n = desktop_entry(&DesktopApp {
            exec: &exec,
            ..app(Gpu::Default)
        })
        .unwrap();
        assert!(d.lines().any(|l| l == "PrefersNonDefaultGPU=true"));
        assert!(!n.contains("PrefersNonDefaultGPU"));
    }

    #[test]
    fn exec_quoting_follows_the_spec() {
        assert_eq!(exec_arg("plain").unwrap(), "plain");
        assert_eq!(exec_arg("a b").unwrap(), "\"a b\"");
        assert_eq!(exec_arg("$x\"`\\").unwrap(), "\"\\$x\\\"\\`\\\\\"");
        assert_eq!(exec_arg("50%").unwrap(), "50%%");
        assert_eq!(exec_arg("").unwrap(), "\"\"");
    }

    #[test]
    fn bad_package_and_control_chars_are_refused() {
        assert!(desktop_file_name("com/evil").is_err());
        assert_eq!(
            desktop_file_name("com.example.game").unwrap(),
            "atl.com.example.game.desktop"
        );
        let exec = ["x".to_owned()];
        assert!(
            desktop_entry(&DesktopApp {
                name: "bad\u{7}",
                exec: &exec,
                ..app(Gpu::Default)
            })
            .is_err()
        );
    }
}
