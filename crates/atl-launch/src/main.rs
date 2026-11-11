// SPDX-License-Identifier: MIT
//! atl-launch: run an ATL app on the chosen GPU, set an app's GPU preference, write its `.desktop` entry (crate docs in lib.rs).
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_launch::{DesktopApp, Gpu, Settings, argv, desktop_entry, desktop_file_name, exec};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run CMD on the chosen GPU. `default`: unchanged. `discrete`: through `switcherooctl launch` (the desktop's offload env); fails, naming it, when switcherooctl is absent.
    Run {
        #[arg(long, value_enum, default_value_t = Gpu::Default)]
        gpu: Gpu,
        /// The ATL command to run, after `--`.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<OsString>,
    },
    /// Store PACKAGE's GPU preference in SETTINGS (TOML, `[apps."<package>"] gpu = ...`).
    SetGpu {
        #[arg(long)]
        settings: PathBuf,
        package: String,
        #[arg(value_enum)]
        gpu: Gpu,
    },
    /// Write `<out>/atl.<package>.desktop`; `PrefersNonDefaultGPU=true` when SETTINGS has the package on `discrete`. A hint: launchers that ignore it (many wlroots/Hyprland ones) start it on the default GPU.
    Desktop {
        #[arg(long)]
        settings: PathBuf,
        /// Directory of the entry, e.g. `~/.local/share/applications`.
        #[arg(long)]
        out: PathBuf,
        /// The entry's display name.
        #[arg(long)]
        name: String,
        package: String,
        /// The command the entry runs, after `--` (written as `atl-launch run -- CMD`).
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::Run { gpu, cmd } => Err(exec(&argv(gpu, &cmd)?).into()),
        Command::SetGpu {
            settings,
            package,
            gpu,
        } => {
            desktop_file_name(&package)?;
            let mut s = Settings::load(&settings)?;
            s.set_gpu(&package, gpu);
            s.save(&settings)
        }
        Command::Desktop {
            settings,
            out,
            name,
            package,
            cmd,
        } => {
            let gpu = Settings::load(&settings)?.gpu(&package);
            let exec: Vec<String> = ["atl-launch", "run", "--"]
                .iter()
                .map(|s| (*s).to_owned())
                .chain(cmd)
                .collect();
            let entry = desktop_entry(&DesktopApp {
                package: &package,
                name: &name,
                exec: &exec,
                gpu,
            })?;
            std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
            let path = out.join(desktop_file_name(&package)?);
            std::fs::write(&path, entry).with_context(|| format!("writing {}", path.display()))
        }
    }
}
