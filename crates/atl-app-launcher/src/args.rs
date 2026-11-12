// SPDX-License-Identifier: MIT
//! The command line: `atl-app-launcher [--target-sdk=<n>] [--app] [--instruction-set=<isa>] [--pipewire-socket=<path>] [--no-audio-service] [--break-cblk] [--no-media-extractor-service] [--no-media-player-service] [--break-mix] <VM option>... <class> [<arg>...]`.
//! VM options are dalvikvm's (art/dalvikvm/dalvikvm.cc): every argument up to the first not starting with `-` goes to `JNI_CreateJavaVM`, `-cp` / `-classpath` taking the next one with it; the class follows, then main's arguments.
//! Launcher options start with `--` and come first; ART takes no option of that form.

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// What to start.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    /// The app's target SDK, for bionic's compatibility checks (`android_set_application_target_sdk_version`); 0 = the platform level, bionic's own meaning of 0.
    pub target_sdk: i32,
    /// An app process (lib.rs app mode): the class runs as zygote's child runs it, not as dalvikvm's main.
    pub app: bool,
    /// The app's instruction set (arches.toml row key), as zygote's `--instruction-set` carries it from the package's `primaryCpuAbi`; the broker passes it for every app process, the bridge's
    /// ISA (lib.rs `native_bridge_options`). None: a process of no package; ART runs its own ISA.
    pub instruction_set: Option<String>,
    /// The audio server's socket atld grants (read-only at this path in the sandbox): the PCM output connects there, and nothing else is looked up. None: none granted; the process starts without audio and its tracks fail.
    pub pipewire_socket: Option<PathBuf>,
    /// Test only: the process serves neither AudioFlinger nor AudioPolicyService (lib.rs serve_media_services), the audio gate's deliberate break.
    pub no_audio_service: bool,
    /// Test only: AudioFlinger's tracks get a control block one buffer short (`atl_audioflinger::Options`), the audio gate's deliberate break.
    pub break_cblk: bool,
    /// Test only: the process serves no media.extractor (lib.rs serve_media_services), the extractor gate's deliberate break.
    pub no_media_extractor_service: bool,
    /// Test only: the process serves no media.player (lib.rs serve_media_services), the media player gate's deliberate break.
    pub no_media_player_service: bool,
    /// Test only: AudioFlinger's mixer plays silence (`atl_audioflinger::Options`), the audio gate's deliberate break.
    pub break_mix: bool,
    pub vm_options: Vec<CString>,
    /// None: none given; dalvikvm reports that after creating the VM (so `-help` and the like run).
    pub class: Option<String>,
    pub args: Vec<CString>,
}

const TARGET_SDK: &str = "--target-sdk=";
const APP: &str = "--app";
const INSTRUCTION_SET: &str = "--instruction-set=";
const PIPEWIRE_SOCKET: &str = "--pipewire-socket=";
const NO_AUDIO_SERVICE: &str = "--no-audio-service";
const BREAK_CBLK: &str = "--break-cblk";
const NO_MEDIA_EXTRACTOR_SERVICE: &str = "--no-media-extractor-service";
const NO_MEDIA_PLAYER_SERVICE: &str = "--no-media-player-service";
const BREAK_MIX: &str = "--break-mix";

pub fn parse(argv: impl IntoIterator<Item = OsString>) -> Result<Launch, String> {
    let mut it = argv
        .into_iter()
        .skip(1)
        .map(|a| CString::new(a.into_vec()).map_err(|e| format!("argument with a NUL byte: {e}")));
    let mut launch = Launch {
        target_sdk: 0,
        app: false,
        instruction_set: None,
        pipewire_socket: None,
        no_audio_service: false,
        break_cblk: false,
        no_media_extractor_service: false,
        no_media_player_service: false,
        break_mix: false,
        vm_options: Vec::new(),
        class: None,
        args: Vec::new(),
    };
    let mut pending = it.next().transpose()?;
    while let Some(arg) = pending.take_if(|a| a.as_bytes().starts_with(b"--")) {
        let text = arg.to_string_lossy();
        if text == APP {
            launch.app = true;
            pending = it.next().transpose()?;
            continue;
        }
        if let Some(isa) = text.strip_prefix(INSTRUCTION_SET) {
            if isa.is_empty() {
                return Err(format!("{INSTRUCTION_SET}: empty"));
            }
            launch.instruction_set = Some(isa.to_owned());
            pending = it.next().transpose()?;
            continue;
        }
        if let Some(path) = arg.as_bytes().strip_prefix(PIPEWIRE_SOCKET.as_bytes()) {
            launch.pipewire_socket = Some(PathBuf::from(OsStr::from_bytes(path)));
            pending = it.next().transpose()?;
            continue;
        }
        if text == NO_AUDIO_SERVICE {
            launch.no_audio_service = true;
            pending = it.next().transpose()?;
            continue;
        }
        if text == BREAK_CBLK {
            launch.break_cblk = true;
            pending = it.next().transpose()?;
            continue;
        }
        if text == NO_MEDIA_EXTRACTOR_SERVICE {
            launch.no_media_extractor_service = true;
            pending = it.next().transpose()?;
            continue;
        }
        if text == NO_MEDIA_PLAYER_SERVICE {
            launch.no_media_player_service = true;
            pending = it.next().transpose()?;
            continue;
        }
        if text == BREAK_MIX {
            launch.break_mix = true;
            pending = it.next().transpose()?;
            continue;
        }
        let value = text
            .strip_prefix(TARGET_SDK)
            .ok_or_else(|| format!("unknown launcher option {text}"))?;
        launch.target_sdk = value
            .parse()
            .map_err(|_| format!("{TARGET_SDK}{value}: not an integer"))?;
        pending = it.next().transpose()?;
    }
    while let Some(arg) = pending.take() {
        if !arg.as_bytes().starts_with(b"-") {
            launch.class = Some(arg.to_string_lossy().into_owned());
            break;
        }
        let takes_value = matches!(arg.as_bytes(), b"-cp" | b"-classpath");
        let name = arg.to_string_lossy().into_owned();
        launch.vm_options.push(arg);
        if takes_value {
            let value = it.next().transpose()?.ok_or_else(|| {
                format!("{name} must be followed by an additional argument giving a value")
            })?;
            launch.vm_options.push(value);
        }
        pending = it.next().transpose()?;
    }
    launch.args = it.collect::<Result<_, _>>()?;
    Ok(launch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Launch, String> {
        parse(
            std::iter::once("launcher")
                .chain(args.iter().copied())
                .map(OsString::from),
        )
    }

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    #[test]
    fn no_audio_service_is_a_launcher_option() {
        let l = p(&["--no-audio-service", "Main"]).unwrap();
        assert!(l.no_audio_service);
        assert_eq!(l.class.as_deref(), Some("Main"));
        assert!(!p(&["Main"]).unwrap().no_audio_service);
    }

    #[test]
    fn break_cblk_is_a_launcher_option() {
        let l = p(&["--break-cblk", "--no-audio-service", "Main"]).unwrap();
        assert!(l.break_cblk && l.no_audio_service);
        assert_eq!(l.class.as_deref(), Some("Main"));
        assert!(!p(&["Main"]).unwrap().break_cblk);
    }

    #[test]
    fn no_media_extractor_service_is_a_launcher_option() {
        let l = p(&["--no-media-extractor-service", "Main"]).unwrap();
        assert!(l.no_media_extractor_service);
        assert!(!p(&["Main"]).unwrap().no_media_extractor_service);
    }

    #[test]
    fn no_media_player_service_is_a_launcher_option() {
        let l = p(&["--no-media-player-service", "Main"]).unwrap();
        assert!(l.no_media_player_service);
        assert_eq!(l.class.as_deref(), Some("Main"));
        assert!(!p(&["Main"]).unwrap().no_media_player_service);
    }

    #[test]
    fn break_mix_is_a_launcher_option() {
        let l = p(&["--break-mix", "Main"]).unwrap();
        assert!(l.break_mix && !l.break_cblk);
        assert!(!p(&["Main"]).unwrap().break_mix);
    }

    #[test]
    fn instruction_set_is_a_launcher_option() {
        let l = p(&["--app", "--instruction-set=arm64", "Main"]).unwrap();
        assert_eq!(l.instruction_set.as_deref(), Some("arm64"));
        assert_eq!(l.class.as_deref(), Some("Main"));
        assert_eq!(p(&["Main"]).unwrap().instruction_set, None);
        assert!(p(&["--instruction-set=", "Main"]).is_err());
    }

    #[test]
    fn pipewire_socket_is_a_launcher_option() {
        let l = p(&["--pipewire-socket=/run/user/1000/pipewire-0", "Main"]).unwrap();
        assert_eq!(
            l.pipewire_socket.as_deref(),
            Some(std::path::Path::new("/run/user/1000/pipewire-0"))
        );
        assert_eq!(l.class.as_deref(), Some("Main"));
        assert_eq!(p(&["Main"]).unwrap().pipewire_socket, None);
    }

    #[test]
    fn dalvikvm_shape() {
        let l = p(&[
            "--target-sdk=30",
            "-Xnoimage-dex2oat",
            "-cp",
            "a.jar",
            "Main",
            "-x",
            "y",
        ])
        .unwrap();
        assert_eq!(l.target_sdk, 30);
        assert!(!l.app);
        assert_eq!(l.vm_options, [c("-Xnoimage-dex2oat"), c("-cp"), c("a.jar")]);
        assert_eq!(l.class.as_deref(), Some("Main"));
        // after the class everything is main's, `-` or not
        assert_eq!(l.args, [c("-x"), c("y")]);
    }

    #[test]
    fn defaults_and_refusals() {
        let l = p(&[
            "--app",
            "--target-sdk=34",
            "android.app.ActivityThread",
            "seq=7",
        ])
        .unwrap();
        assert!(l.app);
        assert_eq!(l.target_sdk, 34);
        assert_eq!(l.args, [c("seq=7")]);
        let l = p(&["-showversion"]).unwrap();
        assert_eq!((l.target_sdk, l.class), (0, None));
        assert!(p(&["-cp"]).unwrap_err().contains("-cp must be followed"));
        assert!(p(&["--target-sdk=x", "Main"]).is_err());
        assert!(
            p(&["--verbose", "Main"])
                .unwrap_err()
                .contains("unknown launcher option")
        );
        // a launcher option after a VM option is a VM option: ART rejects it, as dalvikvm would pass it
        let l = p(&["-Xint", "--target-sdk=1", "Main"]).unwrap();
        assert_eq!(l.vm_options, [c("-Xint"), c("--target-sdk=1")]);
    }
}
