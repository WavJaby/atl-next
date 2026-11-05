// SPDX-License-Identifier: MIT
//! Starting the compositor of an app process, and handing its two service objects and its UI agent (ui_agent.rs) to the registry in the system process (atl_services::compositor_registry has the protocol).
//! Reads from the process's own property area what atld wrote there (`ro.atl.surfaceflinger.suffix`, `ro.atl.display`); a process without them is not an ATL app process and gets no compositor.

use std::sync::Arc;
use std::time::Duration;

use atl_services::compositor_registry::{
    DESCRIPTOR, DISPLAY_PROPERTY, POINTER_PROPERTY, REGISTER, SERVICE, SUFFIX_PROPERTY,
};
use atl_services::display_model::DisplayModel;
use binder::binder_impl::{Binder, BorrowedParcel, IBinderInternal, TransactionCode};
use binder::{Interface, SpIBinder, StatusCode, Strong, declare_binder_interface};

use atl_platform_ui::{PhysSize, PlatformUi};

use crate::host::{Composer, ComposerConfig, ThreadHost};
use crate::input::{self, Message};
use crate::sink::{FirstFrame, ReportSink};
use crate::{service, surface_flinger, ui_agent, ui_window};

/// How long the registry may be missing. atld starts an app process only after the services printed `services: ready`, which follows the registry's addService, so this covers a slow machine, not a race.
const REGISTRY_WAIT: Duration = Duration::from_secs(5);
/// How often the registry is looked up while it is missing.
const REGISTRY_POLL: Duration = Duration::from_millis(50);

pub trait IRegistry: Interface {}

declare_binder_interface! {
    IRegistry[DESCRIPTOR] {
        native: BnRegistry(on_transact),
        proxy: BpRegistry,
    }
}

/// The registry lives in the system process; this side only calls it.
fn on_transact(
    _: &dyn IRegistry,
    _: TransactionCode,
    _: &BorrowedParcel<'_>,
    _: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
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

/// The REGISTER transaction: the suffix, the two service binders, the UI agent.
fn register(
    suffix: &str,
    surface_flinger: &SpIBinder,
    surface_flinger_aidl: &SpIBinder,
    ui_agent: &SpIBinder,
) -> Result<(), String> {
    let proxy = registry()?;
    let binder = proxy.as_binder();
    let mut data = binder.prepare_transact().map_err(|e| e.to_string())?;
    data.write(suffix).map_err(|e| e.to_string())?;
    data.write(surface_flinger).map_err(|e| e.to_string())?;
    data.write(surface_flinger_aidl)
        .map_err(|e| e.to_string())?;
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
    let suffix = property(SUFFIX_PROPERTY)?;
    let display = DisplayModel::parse_arg(&property(DISPLAY_PROPERTY)?)?;
    let script = match optional_property(POINTER_PROPERTY)? {
        Some(script) => input::parse_script(&script)?,
        None => Vec::new(),
    };
    let input_tx = input::spawn(script)?;
    let composer = Composer::new(ComposerConfig {
        display,
        compositor: Default::default(),
    });
    if let Some(ui) = ui {
        let size = PhysSize {
            width: u32::try_from(display.width).unwrap_or(0),
            height: u32::try_from(display.height).unwrap_or(0),
        };
        let frames = composer
            .start_ui(threads)
            .map_err(|e| format!("deliver thread: {e}"))?;
        ui_window::install(ui, frames, input_tx.clone(), size);
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
    }
    let surface_flinger = surface_flinger::new_binder(Arc::clone(&composer));
    let surface_flinger_aidl = service::new_binder(Arc::clone(&composer));
    let ui_agent = ui_agent::new_binder(Arc::clone(&composer), input_tx);
    register(&suffix, &surface_flinger, &surface_flinger_aidl, &ui_agent)?;
    // the registry holds the binders; the process holds the compositor (the clock or deliver thread does)
    Ok(composer)
}
