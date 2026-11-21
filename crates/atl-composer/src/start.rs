// SPDX-License-Identifier: MIT
//! Starting the compositor of an app process: its two service objects are served under AOSP's names by the process's service view (atl-service-view; libgui looks them up there), and its UI agent (ui_agent.rs)
//! goes to the registry in the system process (atl_services_types::compositor_registry has the protocol). Reads from the process's own property area what atld wrote there (`ro.atl.display`); a process without it
//! is not an ATL app process and gets no compositor.

use std::sync::Arc;
use std::time::{Duration, Instant};

use atl_services_types::compositor_registry::{
    DESCRIPTOR, DESKTOP_INPUT_PROPERTY, DISPLAY_PROPERTY, DesktopInput, KEY_MAP, POINTER_PROPERTY,
    REGISTER, SERVICE, SPLASH_PROPERTY, SURFACE_FLINGER, SURFACE_FLINGER_AIDL, Splash, TASK_INFO,
};
use atl_services_types::display_model::DisplayModel;
use binder::binder_impl::{Binder, BorrowedParcel, IBinderInternal, TransactionCode};
use binder::{Interface, SpIBinder, StatusCode, Strong, declare_binder_interface};

use atl_compositor::model::CompositorConfig;
use atl_platform_ui::{LooperPoster, PhysSize, PlatformUi, WindowAttributes};

use crate::host::{Composer, ComposerConfig, Presenter, VmHost};
use crate::input::{self, Message};
use crate::mailbox::Mailbox;
use crate::outbox::Outbox;
use crate::sink::{FirstFrame, ReportSink};
use crate::task_reports::TaskReports;
use crate::ui_window::OpenWindow;
use crate::{service, surface_flinger, ui_agent, ui_window};

/// How long the registry may be missing. atld starts an app process only after the services printed `services: ready`, which follows the registry's addService, so this covers a slow machine, not a race.
const REGISTRY_WAIT: Duration = Duration::from_secs(5);
// Deliberate breaks (a gate failing on purpose) are cargo features `break-<name>` (Cargo.toml), compiled in only by the harness's faulted launcher build (xtask packages.rs `bin_with_breaks`); shipped builds enable none (check step env-reads).
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
    atl_diag::unknown_code("IRegistry", code);
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

/// The TASK_INFO transaction: the size this process's window opens at and its Task's package, from the services (compositor_registry.rs); None for a process with no Task (empty package). With a Task the size must be positive.
fn task_info() -> Result<Option<(PhysSize, String)>, String> {
    let proxy = registry()?;
    let binder = proxy.as_binder();
    let data = binder.prepare_transact().map_err(|e| e.to_string())?;
    let reply = binder
        .submit_transact(TASK_INFO, data, 0)
        .map_err(|e| format!("task info: {e}"))?;
    let width: i32 = reply.read().map_err(|e| format!("task info width: {e}"))?;
    let height: i32 = reply.read().map_err(|e| format!("task info height: {e}"))?;
    let package: String = reply
        .read()
        .map_err(|e| format!("task info package: {e}"))?;
    if package.is_empty() {
        return Ok(None);
    }
    let side = |v: i32| u32::try_from(v).ok().filter(|v| *v > 0);
    match (side(width), side(height)) {
        (Some(width), Some(height)) => Ok(Some((PhysSize { width, height }, package))),
        _ => Err(format!("task info {width}x{height}: not a size")),
    }
}

/// The KEY_MAP transaction: input device `device_id`'s key map in Parcel form, as the input service serves it (compositor_registry.rs KEY_MAP); empty for an unknown device.
pub(crate) fn key_map(device_id: i32) -> Result<Vec<u8>, String> {
    let proxy = registry()?;
    let binder = proxy.as_binder();
    let mut data = binder.prepare_transact().map_err(|e| e.to_string())?;
    data.write(&device_id).map_err(|e| e.to_string())?;
    let reply = binder
        .submit_transact(KEY_MAP, data, 0)
        .map_err(|e| format!("key map {device_id}: {e}"))?;
    reply
        .read()
        .map_err(|e| format!("key map {device_id}: {e}"))
}

/// The REGISTER transaction: the UI agent, then the compositor (GET_COMPOSER hands it to the shell).
fn register(ui_agent: &SpIBinder, composer: &SpIBinder) -> Result<(), String> {
    let proxy = registry()?;
    let binder = proxy.as_binder();
    let mut data = binder.prepare_transact().map_err(|e| e.to_string())?;
    data.write(ui_agent).map_err(|e| e.to_string())?;
    data.write(composer).map_err(|e| e.to_string())?;
    binder
        .submit_transact(REGISTER, data, 0)
        .map_err(|e| format!("register: {e}"))?;
    Ok(())
}

/// UI mode's window, opened before ART boots (launcher step 1d): the services' size and package for this process's Task (TASK_INFO, a main-thread transaction: no thread pool is needed for a synchronous call),
/// the window at that size, so its first bounds report changes nothing, with the app id named before its first commit, showing the starting window from `ro.atl.splash`. `started`: the launcher's start, the origin of the `splash:` lines.
/// No Task (a provider, service or receiver process): no window -- AOSP shows none, and a second toplevel took the desktop focus from the app's.
pub fn open_window(ui: PlatformUi, started: Instant) -> Result<Option<OpenWindow>, String> {
    let Some((size, package)) = task_info()? else {
        return Ok(None);
    };
    let splash = optional_property(SPLASH_PROPERTY)?
        .map(|arg| Splash::parse_arg(&arg))
        .transpose()?;
    // Titled by the package until the first activity's setTaskDescription (onApplyThemeResource, in its create) brings the task label (UI agent TASK_TITLE): resolving that needs the app's Java side.
    let attributes = WindowAttributes {
        title: package.clone(),
        app_id: Some(atl_launch::app_id(&package).map_err(|e| e.to_string())?),
        size,
    };
    // TASK_INFO above is a binder call: it stays on this app thread, never on the display thread (display-thread plan D2 rule B1); the window is made there by `call`
    ui_window::open(ui, attributes, splash, started).map(Some)
}

/// Starts this process's compositor and registers it. Call it with libbinder's thread pool running (the registry's reply and libgui's calls arrive on its threads) and before anything uses libgui.
/// The clock thread and the present thread come from `host` in both modes; the UI agent keeps it for the window title's label lookup. Headless (`window` None): scenes go to a report sink, and a scripted pointer goes in at the first frame. UI mode (`window`: the Task's window
/// [`open_window`] opened): scenes go through a mailbox to it ([`ui_window`]), which takes the latest on its frame clock; the window's session is installed on the display thread (`call`) before the composer registers.
pub fn start(host: Arc<dyn VmHost>, window: Option<OpenWindow>) -> Result<Arc<Composer>, String> {
    let threads: &dyn VmHost = &*host;
    let display = DisplayModel::parse_arg(&property(DISPLAY_PROPERTY)?)?;
    let script = match optional_property(POINTER_PROPERTY)? {
        Some(script) => input::parse_script(&script)?,
        None => Vec::new(),
    };
    // break `input-no-wake`: the input loop is never woken by a send (input::spawn): messages queue, and the loop sleeps through them
    let wake_enabled = !cfg!(feature = "break-input-no-wake");
    let desktop_input = match optional_property(DESKTOP_INPUT_PROPERTY)? {
        Some(arg) => DesktopInput::parse_arg(&arg)?,
        None => DesktopInput::default(),
    };
    let tilt = match desktop_input.tilt {
        Some(config) => {
            let t = Arc::new(std::sync::Mutex::new(crate::tilt::Tilt::new(config)));
            crate::tilt::spawn(Arc::clone(&t), display.rotation)?;
            Some(t)
        }
        None => None,
    };
    let input_tx = input::spawn(script, wake_enabled, tilt, desktop_input.mouse_touch)?;
    let composer = Composer::new(ComposerConfig {
        display,
        compositor: CompositorConfig {
            // break `z-order`: siblings ignore their z; break `queue`: no queued buffer is latched
            z_order: !cfg!(feature = "break-z-order"),
            latch_buffers: !cfg!(feature = "break-queue"),
            ..Default::default()
        },
        // break `post-draw`: the window service's forwarded post-draw Transactions are dropped; break `ignore-hold`: a shown buffer is released at the latch that replaces it;
        // break `no-backpressure`: every tick latches, whether the window took the last scene or not
        post_draw: !cfg!(feature = "break-post-draw"),
        hold_shown: !cfg!(feature = "break-ignore-hold"),
        backpressure: !cfg!(feature = "break-no-backpressure"),
    });
    // the in-app input method exists only with a window: its desktop side is the window's TextInput (headless: none, IMMS-lite answers NO_IME)
    let mut input_method = None;
    let reports = if let Some(window) = window {
        let invoker = window
            .invoker()
            .ok_or_else(|| "ui window: already installed".to_owned())?;
        input_method = Some(atl_ime::new_binder(Arc::new(invoker)));
        let mailbox = Arc::new(Mailbox::default());
        composer
            .start(threads, Presenter::Window(Arc::clone(&mailbox)))
            .map_err(|e| format!("frame clock thread: {e}"))?;
        let outbox: Arc<dyn LooperPoster> = Arc::new(Outbox::spawn(threads)?);
        let reports = TaskReports::new(Arc::clone(&outbox));
        ui_window::install(
            window,
            Arc::clone(&composer),
            mailbox,
            input_tx.clone(),
            Arc::clone(&reports),
            outbox,
        )?;
        Some(reports)
    } else {
        let first_frame = input_tx.clone();
        composer
            .start(
                threads,
                Presenter::Sink(Box::new(FirstFrame::new(
                    ReportSink(std::io::stderr()),
                    move || {
                        // the script goes in at the first frame; the input thread owns the publisher
                        let _ = first_frame.send(Message::FirstFrame);
                    },
                ))),
            )
            .map_err(|e| format!("frame clock thread: {e}"))?;
        None
    };
    let surface_flinger = surface_flinger::new_binder(Arc::clone(&composer));
    let surface_flinger_aidl = service::new_binder(Arc::clone(&composer));
    // the registry gets its own reference: the service view takes the object below
    let registered_composer = surface_flinger_aidl.clone();
    let ui_agent = ui_agent::new_binder(
        Arc::clone(&composer),
        input_tx,
        reports,
        input_method,
        Arc::clone(&host),
    );
    if cfg!(feature = "break-no-serve") {
        // break `no-serve` (xtask composer `--break-view`): the binders die with this frame, nothing serves the names, so libgui's lookup must fail the gate
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
    register(&ui_agent, &registered_composer)?;
    // the service view holds the binders, the registry the agent; the process holds the compositor (the clock or deliver thread does)
    Ok(composer)
}
