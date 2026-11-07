// SPDX-License-Identifier: MIT
//! Starting the compositor of an app process: its two service objects are served under AOSP's names by the process's service view (atl-service-view; libgui looks them up there), and its UI agent (ui_agent.rs)
//! goes to the registry in the system process (atl_services::compositor_registry has the protocol). Reads from the process's own property area what atld wrote there (`ro.atl.display`); a process without it
//! is not an ATL app process and gets no compositor.

use std::sync::Arc;
use std::time::Duration;

use atl_services::compositor_registry::{
    DESCRIPTOR, DISPLAY_PROPERTY, POINTER_PROPERTY, REGISTER, SERVICE, SURFACE_FLINGER,
    SURFACE_FLINGER_AIDL,
};
use atl_services::counters;
use atl_services::display_model::DisplayModel;
use binder::binder_impl::{Binder, BorrowedParcel, IBinderInternal, TransactionCode};
use binder::{Interface, SpIBinder, StatusCode, Strong, declare_binder_interface};

use atl_compositor::model::CompositorConfig;
use atl_platform_ui::{PhysSize, PlatformUi, Ui};

use crate::host::{Composer, ComposerConfig, ThreadHost};
use crate::input::{self, Message};
use crate::sink::{FirstFrame, ReportSink};
use crate::task_sender::TaskSender;
use crate::{service, surface_flinger, ui_agent, ui_window};

/// How long the registry may be missing. atld starts an app process only after the services printed `services: ready`, which follows the registry's addService, so this covers a slow machine, not a race.
const REGISTRY_WAIT: Duration = Duration::from_secs(5);
/// A deliberate-failure run's broken rule (set by xtask's window gate only): with [`BREAK_Z_ORDER`] siblings ignore their z (CompositorConfig::z_order).
pub(crate) const BREAK_ENV: &str = "ATL_UI_BREAK";
/// With it the windows' frames never reach the input thread (ui_agent input_frame): pointer events stay in Task coordinates.
pub(crate) const BREAK_INPUT_FRAMES: &str = "input-frames";
const BREAK_Z_ORDER: &str = "z-order";
/// With it the window service's forwarded post-draw Transactions are dropped (ComposerConfig::post_draw).
const BREAK_POST_DRAW: &str = "post-draw";
/// With it no queued buffer is latched (CompositorConfig::latch_buffers).
const BREAK_QUEUE: &str = "queue";
/// With it a tick's calls go out before the window painted (ComposerConfig::release_after_paint).
const BREAK_RELEASE_BEFORE_PAINT: &str = "release-before-paint";
/// With it the compositor is not served by the service view (xtask composer `--break-view`): libgui's lookup must then fail the gate.
const BREAK_NO_SERVE: &str = "no-serve";
/// With it the input loop is never woken by a send (input::spawn): messages queue, and the loop sleeps through them.
const BREAK_INPUT_NO_WAKE: &str = "input-no-wake";
/// How often the registry is looked up while it is missing.
const REGISTRY_POLL: Duration = Duration::from_millis(50);

pub(crate) trait IRegistry: Interface {}

declare_binder_interface! {
    IRegistry[DESCRIPTOR] {
        native: BnRegistry(on_transact),
        proxy: BpRegistry,
    }
}

/// The registry lives in the system process; this side only calls it.
fn on_transact(
    _: &dyn IRegistry,
    code: TransactionCode,
    _: &BorrowedParcel<'_>,
    _: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    counters::count_unimplemented("IRegistry", &format!("code {code}"));
    Err(StatusCode::UNKNOWN_TRANSACTION)
}

impl IRegistry for BpRegistry {}
impl IRegistry for Binder<BnRegistry> {}

/// A system property of the process: None when unset or empty.
fn optional_property(name: &str) -> Result<Option<String>, String> {
    rustutils::system_properties::read(name)
        .map(|v| v.filter(|v| !v.is_empty()))
        .map_err(|e| format!("reading {name}: {e}"))
}

fn property(name: &str) -> Result<String, String> {
    optional_property(name)?
        .ok_or_else(|| format!("{name} is not set: not an app process atld started"))
}

fn registry() -> Result<Strong<dyn IRegistry>, String> {
    let deadline = std::time::Instant::now() + REGISTRY_WAIT;
    loop {
        if let Some(binder) = binder::check_service(SERVICE) {
            return binder
                .into_interface()
                .map_err(|e| format!("{SERVICE}: not the registry: {e}"));
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("{SERVICE}: not registered after {REGISTRY_WAIT:?}"));
        }
        std::thread::sleep(REGISTRY_POLL);
    }
}

/// The REGISTER transaction: the UI agent.
fn register(ui_agent: &SpIBinder) -> Result<(), String> {
    let proxy = registry()?;
    let binder = proxy.as_binder();
    let mut data = binder.prepare_transact().map_err(|e| e.to_string())?;
    data.write(ui_agent).map_err(|e| e.to_string())?;
    binder
        .submit_transact(REGISTER, data, 0)
        .map_err(|e| format!("register: {e}"))?;
    Ok(())
}

/// Starts this process's compositor and registers it. Call it with libbinder's thread pool running (the registry's reply and libgui's calls arrive on its threads) and before anything uses libgui.
/// Headless (`ui` None): the clock thread and the present thread come from `threads`, and a scripted pointer goes in at the first frame. UI mode (`ui`: the toolkit this UI thread attached): the window's frame clock drives the
/// compositor and the display is a GTK window ([`ui_window`]); only the deliver thread comes from `threads`. The caller is the UI thread in UI mode.
pub fn start(threads: &dyn ThreadHost, ui: Option<PlatformUi>) -> Result<Arc<Composer>, String> {
    let display = DisplayModel::parse_arg(&property(DISPLAY_PROPERTY)?)?;
    let script = match optional_property(POINTER_PROPERTY)? {
        Some(script) => input::parse_script(&script)?,
        None => Vec::new(),
    };
    let wake_enabled = std::env::var(BREAK_ENV).as_deref() != Ok(BREAK_INPUT_NO_WAKE);
    let input_tx = input::spawn(script, wake_enabled)?;
    let composer = Composer::new(ComposerConfig {
        display,
        compositor: CompositorConfig {
            z_order: std::env::var(BREAK_ENV).as_deref() != Ok(BREAK_Z_ORDER),
            latch_buffers: std::env::var(BREAK_ENV).as_deref() != Ok(BREAK_QUEUE),
            ..Default::default()
        },
        post_draw: std::env::var(BREAK_ENV).as_deref() != Ok(BREAK_POST_DRAW),
        release_after_paint: std::env::var(BREAK_ENV).as_deref() != Ok(BREAK_RELEASE_BEFORE_PAINT),
    });
    // the in-app input method exists only with a window: its desktop side is the window's TextInput (headless: none, IMMS-lite answers NO_IME)
    let mut input_method = None;
    let sender = if let Some(ui) = ui {
        input_method = Some(atl_ime::new_binder(Arc::new(ui.invoker())));
        // the size the services give a new Task: the window opens at it, so its first bounds report changes nothing
        let (width, height) = display.opening_size();
        let size = PhysSize {
            width: u32::try_from(width).unwrap_or(1),
            height: u32::try_from(height).unwrap_or(1),
        };
        let frames = composer
            .start_ui(threads)
            .map_err(|e| format!("deliver thread: {e}"))?;
        let sender = TaskSender::spawn(threads).map_err(|e| format!("task sender thread: {e}"))?;
        ui_window::install(ui, frames, input_tx.clone(), size, Arc::clone(&sender));
        Some(sender)
    } else {
        let first_frame = input_tx.clone();
        composer
            .start(
                threads,
                Box::new(FirstFrame::new(ReportSink(std::io::stderr()), move || {
                    // the script goes in at the first frame; the input thread owns the publisher
                    let _ = first_frame.send(Message::FirstFrame);
                })),
            )
            .map_err(|e| format!("frame clock thread: {e}"))?;
        None
    };
    let surface_flinger = surface_flinger::new_binder(Arc::clone(&composer));
    let surface_flinger_aidl = service::new_binder(Arc::clone(&composer));
    let ui_agent = ui_agent::new_binder(Arc::clone(&composer), input_tx, sender, input_method);
    if std::env::var(BREAK_ENV).as_deref() == Ok(BREAK_NO_SERVE) {
        // the deliberate failure: the binders die with this frame, nothing serves the names
        drop((surface_flinger, surface_flinger_aidl));
    } else {
        atl_service_view::serve(SURFACE_FLINGER, surface_flinger)?;
        atl_service_view::serve(SURFACE_FLINGER_AIDL, surface_flinger_aidl)?;
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "composer: serving {SURFACE_FLINGER}, {SURFACE_FLINGER_AIDL} in-process"
        );
    }
    register(&ui_agent)?;
    // the service view holds the binders, the registry the agent; the process holds the compositor (the clock or deliver thread does)
    Ok(composer)
}
