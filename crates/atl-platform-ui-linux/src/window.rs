// SPDX-License-Identifier: MIT
//! `GtkWindow`: a `GtkWindow` whose only content is the `SceneView`, with its GdkFrameClock as the `FrameClock` and pointer controllers as the `InputSource`.

use std::cell::{Cell, RefCell};
use std::os::fd::AsFd;
use std::rc::Rc;

use atl_gralloc_dmabuf::{SYNC_WRITE, export_sync_file};
use atl_platform_ui_api::{
    Capture, CursorShape, FrameClock, FrameTick, InputEvent, InputSource, Locks, Monitor, Painted,
    PhysSize, PointerPhase, PresentError, PresentTiming, QuadRelease, Scale, Scene, TouchPhase,
    UiError, Window, WindowAttributes,
};
use gtk4::glib::translate::ToGlibPtr;
use gtk4::prelude::*;
use gtk4::{gdk, glib, graphene, gsk};

use crate::input;
use crate::mirror;
use crate::outputs;
use crate::scene;
use crate::text_input::GtkTextInput;
use crate::ui::GtkUi;
use crate::view::{SceneView, logical_px};

/// GDK frame times are microseconds (`g_get_monotonic_time`); ticks and presentation samples are nanoseconds.
const NS_PER_US: i64 = 1000;

/// A callback slot that tolerates the callback replacing itself while it runs: it is taken out for the call and put back unless a new one was set.
pub(crate) struct Slot<T: ?Sized>(RefCell<Option<Box<T>>>);

impl<T: ?Sized> Slot<T> {
    pub(crate) fn new() -> Self {
        Slot(RefCell::new(None))
    }

    pub(crate) fn set(&self, f: Box<T>) {
        *self.0.borrow_mut() = Some(f);
    }

    pub(crate) fn call(&self, run: impl FnOnce(&mut T)) {
        let taken = self.0.borrow_mut().take();
        if let Some(mut f) = taken {
            run(&mut f);
            let mut slot = self.0.borrow_mut();
            if slot.is_none() {
                *slot = Some(f);
            }
        }
    }
}

/// State shared between the window handle and the toolkit's signal closures.
struct Shared {
    on_tick: Slot<dyn FnMut(FrameTick)>,
    frame_requested: Cell<bool>,
    on_event: Slot<dyn FnMut(InputEvent)>,
    on_resize: Slot<dyn FnMut(PhysSize)>,
    on_monitor: Slot<dyn FnMut(Monitor)>,
    /// The monitor entered last, with the handlers of its change notifications (disconnected when another is entered).
    monitor: RefCell<Option<(gdk::Monitor, Vec<glib::SignalHandlerId>)>>,
    /// The pointer's last position over the content (logical pixels): GTK's leave and scroll signals carry none.
    pointer_at: Cell<(f64, f64)>,
    /// A touchpad swipe is open: its end (GTK `scroll-end`) is reported; a wheel's is not.
    swiping: Cell<bool>,
    /// Contact ids of the touch sequences down.
    touch_ids: RefCell<input::TouchIds>,
    on_painted: Slot<dyn FnMut(Painted)>,
    on_presented: Slot<dyn FnMut(PresentTiming)>,
    /// The frame counter of the last frame reported to `on_presented`; None before the first report.
    presented_upto: Cell<Option<i64>>,
    /// The buffers the presented scene samples.
    shown: RefCell<Vec<scene::Shown>>,
    /// Buffers a present replaced: GSK may still draw them until the next paint; released at its after-paint.
    retired: RefCell<Vec<scene::Shown>>,
    /// A present waits for its paint: the next after-paint reports.
    paint_pending: Cell<bool>,
}

impl Shared {
    /// After-paint of a frame that followed a present: the buffers that frame no longer shows go back, each with the reservation fences of its dma-buf as release fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` for a write: every pending GPU read of it, by this paint or an earlier one; design §3.4).
    /// A driver that attaches no implicit fences (WSL: GSK on llvmpipe, which finishes rendering before the swap) yields an already-signalled fence.
    fn painted(&self) {
        if !self.paint_pending.replace(false) {
            return;
        }
        let retired = std::mem::take(&mut *self.retired.borrow_mut());
        let releases = retired
            .into_iter()
            .map(|s| {
                let release_fence = match export_sync_file(s.dmabuf.as_fd(), SYNC_WRITE) {
                    Ok(fence) => Some(fence),
                    Err(e) => {
                        tracing::warn!(
                            "atl-platform-ui: layer {} buffer {:#x}: no read fence: {e}",
                            s.layer_id,
                            s.buffer_key
                        );
                        None
                    }
                };
                QuadRelease {
                    layer_id: s.layer_id,
                    buffer_key: s.buffer_key,
                    release_fence,
                }
            })
            .collect();
        self.on_painted.call(|f| f(Painted { releases }));
    }

    /// Reports the frames GDK completed since the last report, in frame order, as presentation samples. A frame whose feedback has not come is skipped, not waited for: a frame that
    /// committed nothing never gets feedback, so waiting would stall every later report. Samples with no presentation time or no period are not reported.
    fn report_presented(&self, clock: &gdk::FrameClock) {
        let first = self
            .presented_upto
            .get()
            .map_or(clock.history_start(), |last| {
                (last + 1).max(clock.history_start())
            });
        for frame in first..=clock.frame_counter() {
            let Some(timings) = clock.timings(frame) else {
                continue;
            };
            let (presented, period) = (timings.presentation_time(), timings.refresh_interval());
            if !timings.is_complete() || presented <= 0 || period <= 0 {
                continue;
            }
            self.presented_upto.set(Some(frame));
            let sample = PresentTiming {
                time_ns: presented * NS_PER_US,
                refresh_ns: period * NS_PER_US,
            };
            self.on_presented.call(|f| f(sample));
        }
    }

    /// One requested tick at `time_us`: the request is spent and the callback runs. No request, no tick: the clock is on demand.
    fn tick(&self, time_us: i64) {
        self.frame_requested.set(false);
        let time_ns = u64::try_from(time_us * NS_PER_US).unwrap_or(0);
        self.on_tick.call(|f| f(FrameTick { time_ns }));
    }
}

/// A toplevel window on GTK4.
pub struct GtkWindow {
    window: gtk4::Window,
    view: SceneView,
    /// The size last asked for, until the compositor has granted one.
    requested: Cell<PhysSize>,
    shared: Rc<Shared>,
    /// The content origin is logged once, at the first present with the window mapped.
    origin_logged: Cell<bool>,
    text_input: GtkTextInput,
    /// The output-transform listener (outputs.rs): removed with the window, so a closed window leaves no callback behind.
    _output_follow: outputs::Follow,
}

impl GtkWindow {
    /// The content's origin in the surface, where pointer input is injected: the native's surface transform (the client-side shadow) plus the view's place in the window (the header bar).
    /// GTK's controllers already report content-local points, so a surface point is content-local at `point - origin`.
    fn content_origin(&self) -> Option<(f64, f64)> {
        let (shadow_x, shadow_y) = self.window.surface_transform();
        let at = self
            .view
            .compute_point(&self.window, &graphene::Point::new(0.0, 0.0))?;
        Some((shadow_x + f64::from(at.x()), shadow_y + f64::from(at.y())))
    }

    fn wire_frame_clock(window: &gtk4::Window, shared: &Rc<Shared>) {
        let shared = Rc::clone(shared);
        window.connect_realize(move |window| {
            let Some(clock) = window.frame_clock() else {
                tracing::warn!("realised window has no frame clock");
                return;
            };
            let on_update = Rc::clone(&shared);
            clock.connect_update(move |clock| {
                if on_update.frame_requested.get() {
                    on_update.tick(clock.frame_time());
                }
            });
            let on_after_paint = Rc::clone(&shared);
            clock.connect_after_paint(move |clock| {
                on_after_paint.painted();
                on_after_paint.report_presented(clock);
            });
            // a frame asked for before the window had a clock
            if shared.frame_requested.get() {
                clock.request_phase(gdk::FrameClockPhase::UPDATE);
            }
        });
    }

    fn wire_pointer(view: &SceneView, shared: &Rc<Shared>) {
        fn emit(
            shared: &Shared,
            controller: &impl IsA<gtk4::EventController>,
            phase: PointerPhase,
            x: f64,
            y: f64,
        ) {
            shared.pointer_at.set((x, y));
            let event = input::pointer(
                phase,
                x,
                y,
                scale_of(controller),
                controller.current_event_time(),
            );
            shared.on_event.call(|f| f(event));
        }
        // button 0: every button
        let click = gtk4::GestureClick::new();
        click.set_button(0);
        let s = Rc::clone(shared);
        click.connect_pressed(move |g, _, x, y| {
            emit(
                &s,
                g,
                PointerPhase::Down(input::button(g.current_button())),
                x,
                y,
            );
        });
        let s = Rc::clone(shared);
        click.connect_released(move |g, _, x, y| {
            emit(
                &s,
                g,
                PointerPhase::Up(input::button(g.current_button())),
                x,
                y,
            );
        });
        view.add_controller(click);
        let motion = gtk4::EventControllerMotion::new();
        let s = Rc::clone(shared);
        motion.connect_motion(move |c, x, y| emit(&s, c, PointerPhase::Move, x, y));
        let s = Rc::clone(shared);
        motion.connect_enter(move |c, x, y| emit(&s, c, PointerPhase::Enter, x, y));
        let s = Rc::clone(shared);
        motion.connect_leave(move |c| {
            let (x, y) = s.pointer_at.get();
            emit(&s, c, PointerPhase::Leave, x, y);
        });
        view.add_controller(motion);
    }

    /// Wheel and touchpad scrolling. Not KINETIC (Android flings itself from the swipe's own events, GestureConverter.cpp handleFling) and not DISCRETE (hi-res wheels report fractions).
    /// The unit tells a wheel from fingers; GDK gives finger scrolling its own device, so `scroll-end` marks the fingers lifting (wl_pointer.axis_stop).
    fn wire_scroll(view: &SceneView, shared: &Rc<Shared>) {
        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        let s = Rc::clone(shared);
        scroll.connect_scroll(move |c, dx, dy| {
            let unit = match c.unit() {
                gdk::ScrollUnit::Wheel => input::ScrollUnit::Wheel,
                _ => input::ScrollUnit::Surface,
            };
            s.swiping.set(unit == input::ScrollUnit::Surface);
            let event = input::scroll(
                unit,
                dx,
                dy,
                s.pointer_at.get(),
                scale_of(c),
                c.current_event_time(),
            );
            s.on_event.call(|f| f(event));
            gtk4::glib::Propagation::Stop
        });
        let s = Rc::clone(shared);
        scroll.connect_scroll_end(move |c| {
            if s.swiping.replace(false) {
                let event =
                    input::swipe_end(s.pointer_at.get(), scale_of(c), c.current_event_time());
                s.on_event.call(|f| f(event));
            }
        });
        view.add_controller(scroll);
    }

    /// Touch contacts, each sequence its own contact: GTK4 has no multi-touch controller, so the raw events are taken in the capture phase and stopped there -- GestureClick would otherwise claim a touch as a primary-button click.
    fn wire_touch(view: &SceneView, shared: &Rc<Shared>) {
        let legacy = gtk4::EventControllerLegacy::new();
        legacy.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let s = Rc::clone(shared);
        legacy.connect_event(move |c, event| {
            let phase = match event.event_type() {
                gdk::EventType::TouchBegin => TouchPhase::Down,
                gdk::EventType::TouchUpdate => TouchPhase::Move,
                gdk::EventType::TouchEnd => TouchPhase::Up,
                gdk::EventType::TouchCancel => TouchPhase::Cancel,
                _ => return glib::Propagation::Proceed,
            };
            let sequence: *mut gdk::ffi::GdkEventSequence = event.event_sequence().to_glib_none().0;
            let id = s.touch_ids.borrow_mut().id(sequence as usize, phase);
            // the event's position is the surface's; the content's is past the shadow and the header bar
            let at = c.widget().zip(event.position()).and_then(|(view, (x, y))| {
                let native = view.native()?;
                let (shadow_x, shadow_y) = native.surface_transform();
                native.compute_point(
                    &view,
                    &graphene::Point::new((x - shadow_x) as f32, (y - shadow_y) as f32),
                )
            });
            if let (Some(id), Some(at)) = (id, at) {
                let event = input::touch(
                    phase,
                    id,
                    f64::from(at.x()),
                    f64::from(at.y()),
                    scale_of(c),
                    c.current_event_time(),
                );
                s.on_event.call(|f| f(event));
            }
            glib::Propagation::Stop
        });
        view.add_controller(legacy);
    }

    /// Keys of the toplevel (the window has keyboard focus): presses, GDK's synthesized repeats (the desktop's `repeat_info`), releases. The IME item puts its GtkIMContext on this controller, so keys it consumes never arrive here.
    fn wire_keys(window: &gtk4::Window, shared: &Rc<Shared>) {
        let keys = gtk4::EventControllerKey::new();
        let send = |s: &Rc<Shared>, c: &gtk4::EventControllerKey, down: bool, keycode: u32| {
            let locks = c
                .current_event_device()
                .map(|d| Locks {
                    caps: d.is_caps_locked(),
                    num: d.is_num_locked(),
                    scroll: d.is_scroll_locked(),
                })
                .unwrap_or_default();
            if let Some(event) = input::key(down, keycode, locks, c.current_event_time()) {
                s.on_event.call(|f| f(event));
            }
        };
        let s = Rc::clone(shared);
        keys.connect_key_pressed(move |c, _, keycode, _| {
            send(&s, c, true, keycode);
            glib::Propagation::Stop
        });
        let s = Rc::clone(shared);
        keys.connect_key_released(move |c, _, keycode, _| send(&s, c, false, keycode));
        window.add_controller(keys);
    }
}

/// Sizes the window to the content's wanted physical size at the surface's current scale, keeping the chrome around the content. Runs at each change of either: the window keeps its integer-scale size otherwise, when the fractional scale arrives after the first layout (Hyprland, 1.6).
fn fit_window(window: &gtk4::Window, view: &SceneView) {
    if !(window.is_mapped() && window.surface().is_some_and(|s| s.is_mapped())) {
        return;
    }
    let wanted = view.wanted();
    let scale = view.scale();
    let (width, height) = (
        logical_px(scale, wanted.width),
        logical_px(scale, wanted.height),
    );
    // the window is the content plus the chrome around it: keep the chrome, change the content
    let (chrome_w, chrome_h) = (
        window.width() - view.width(),
        window.height() - view.height(),
    );
    tracing::info!(
        "atl-platform-ui: window fit to {}x{} logical at scale {}, content {}x{} physical",
        width + chrome_w,
        height + chrome_h,
        scale.get(),
        wanted.width,
        wanted.height,
    );
    window.set_default_size(width + chrome_w, height + chrome_h);
}

/// The scale of the controller's `SceneView` (1 before it is attached).
fn scale_of(controller: &impl IsA<gtk4::EventController>) -> Scale {
    controller
        .widget()
        .and_then(|w| w.downcast::<SceneView>().ok())
        .map_or(Scale::ONE, |v| v.scale())
}

impl Window for GtkWindow {
    type Ui = GtkUi;
    type TextInput = GtkTextInput;

    fn text_input(&self) -> &GtkTextInput {
        &self.text_input
    }

    fn create(ui: &GtkUi, attributes: &WindowAttributes) -> Result<Self, UiError> {
        ui.check_thread()?;
        // GTK takes the xdg app_id from the program name at realise (inside `present`): the initial commit carries it and the compositor matches window rules on that commit, so it is set first. Process-wide: valid while one Android package runs per app process; a shared-uid second package keeps the first's id.
        if let Some(app_id) = &attributes.app_id {
            glib::set_prgname(Some(app_id.as_str()));
        }
        let shared = Rc::new(Shared {
            on_tick: Slot::new(),
            frame_requested: Cell::new(false),
            on_event: Slot::new(),
            on_resize: Slot::new(),
            pointer_at: Cell::new((0.0, 0.0)),
            swiping: Cell::new(false),
            touch_ids: RefCell::default(),
            on_monitor: Slot::new(),
            monitor: RefCell::new(None),
            on_painted: Slot::new(),
            on_presented: Slot::new(),
            presented_upto: Cell::new(None),
            shown: RefCell::default(),
            retired: RefCell::default(),
            paint_pending: Cell::new(false),
        });
        // the output transform is not GDK's (outputs.rs): its change re-reports the window's monitor
        let rotated = Rc::downgrade(&shared);
        let output_follow = outputs::follow(move || {
            let Some(shared) = rotated.upgrade() else {
                return;
            };
            let monitor = shared.monitor.borrow().as_ref().map(|(m, _)| m.clone());
            if let Some(m) = monitor {
                report_monitor(&shared, &m);
            }
        });
        let window = gtk4::Window::new();
        let view = SceneView::new();
        Self::wire_frame_clock(&window, &shared);
        Self::wire_pointer(&view, &shared);
        Self::wire_scroll(&view, &shared);
        Self::wire_touch(&view, &shared);
        Self::wire_keys(&window, &shared);
        // the monitor the window is on (wl_surface.enter): reported at each entry and at each change of that monitor (Window::on_monitor)
        // and its scale (wp_fractional_scale_v1 preferred_scale arrives after realisation): the content's logical size is re-derived from the physical one
        let entered = Rc::clone(&shared);
        let scaled = (view.downgrade(), window.downgrade());
        window.connect_realize(move |w| {
            if let Some(surface) = w.surface() {
                let entered = Rc::clone(&entered);
                surface.connect_enter_monitor(move |_, m| follow_monitor(&entered, m));
                let scaled = scaled.clone();
                surface.connect_scale_notify(move |s| {
                    tracing::info!("atl-platform-ui: surface scale {}", s.scale());
                    // from the next idle, not inside the notification: GTK dropped a default-size request made there (the window kept its integer-scale size)
                    let scaled = scaled.clone();
                    glib::idle_add_local_once(move || {
                        if let (Some(view), Some(window)) = (scaled.0.upgrade(), scaled.1.upgrade())
                        {
                            view.rescale();
                            fit_window(&window, &view);
                            mirror::resync();
                        }
                    });
                });
                // the xdg-shell states GDK reports: `suspended` is the hidden signal the window lifecycle consumes (S2); logged per change
                if let Ok(toplevel) = surface.downcast::<gdk::Toplevel>() {
                    toplevel.connect_state_notify(|t| {
                        let state = t.state();
                        tracing::info!(
                            "atl-platform-ui: state maximized={} suspended={}",
                            state.contains(gdk::ToplevelState::MAXIMIZED),
                            state.contains(gdk::ToplevelState::SUSPENDED),
                        );
                    });
                }
            }
        });
        // keyboard focus = the active toplevel (xdg_toplevel state `activated`); GTK notifies each change
        let focused = Rc::clone(&shared);
        window.connect_is_active_notify(move |w| {
            let active = w.is_active();
            tracing::info!("atl-platform-ui: focus {active}");
            focused.on_event.call(|f| f(InputEvent::Focus(active)));
        });
        let resized = Rc::clone(&shared);
        view.connect_allocation(move |size| resized.on_resize.call(|f| f(size)));
        window.set_child(Some(&view));
        let text_input = GtkTextInput::new(&view);
        let this = GtkWindow {
            _output_follow: output_follow,
            window,
            view,
            requested: Cell::new(attributes.size),
            shared,
            origin_logged: Cell::new(false),
            text_input,
        };
        this.set_title(&attributes.title);
        this.set_size(attributes.size);
        this.window.present();
        mirror::resync();
        Ok(this)
    }

    fn set_size(&self, size: PhysSize) {
        self.requested.set(size);
        self.view.set_wanted(size);
        fit_window(&self.window, &self.view);
        // every GTK change this layer makes may attach a source (a layout or paint request of the frame clock): from outside a mirror pass it reaches the Looper only through a sync. The platform layer is the only GTK caller, so its entry points are the complete set.
        mirror::resync();
    }

    fn set_title(&self, title: &str) {
        self.window.set_title(Some(title));
        mirror::resync();
    }

    /// On the content view: GDK sets it through `wp_cursor_shape_v1` where the compositor has it, else a themed cursor surface.
    fn set_cursor(&self, shape: CursorShape) {
        let name = input::cursor_name(shape);
        tracing::info!("atl-platform-ui: cursor {name}");
        self.view.set_cursor_from_name(Some(name));
        mirror::resync();
    }

    fn size(&self) -> PhysSize {
        let (width, height) = (self.view.width(), self.view.height());
        if width <= 0 || height <= 0 {
            return self.requested.get();
        }
        let scale = self.view.scale();
        PhysSize {
            width: scale.to_physical(f64::from(width)).round() as u32,
            height: scale.to_physical(f64::from(height)).round() as u32,
        }
    }

    fn scale(&self) -> Scale {
        self.view.scale()
    }

    fn is_mapped(&self) -> bool {
        self.window.is_mapped() && self.window.surface().is_some_and(|s| s.is_mapped())
    }

    /// GdkToplevelState as GDK's Wayland backend sets it from the last acked xdg_toplevel.configure's states (any tiled edge counts).
    fn is_size_fixed(&self) -> bool {
        let fixed = gdk::ToplevelState::MAXIMIZED
            | gdk::ToplevelState::FULLSCREEN
            | gdk::ToplevelState::TILED
            | gdk::ToplevelState::TOP_TILED
            | gdk::ToplevelState::RIGHT_TILED
            | gdk::ToplevelState::BOTTOM_TILED
            | gdk::ToplevelState::LEFT_TILED;
        self.window
            .surface()
            .and_then(|s| s.downcast::<gdk::Toplevel>().ok())
            .is_some_and(|t| t.state().intersects(fixed))
    }

    fn present(&self, scene: &Scene) -> Result<(), PresentError> {
        let (node, shown) = scene::build(&WidgetExt::display(&self.window), scene)?;
        self.view.show_node(node);
        // a buffer still in the new scene stays shown; the rest is retired until the paint that stops drawing it
        let old = std::mem::replace(&mut *self.shared.shown.borrow_mut(), shown);
        let still = |s: &scene::Shown| {
            self.shared
                .shown
                .borrow()
                .iter()
                .any(|n| n.buffer_key == s.buffer_key)
        };
        let gone: Vec<_> = old.into_iter().filter(|s| !still(s)).collect();
        let mut retired = self.shared.retired.borrow_mut();
        // retired by an earlier present of this frame and shown again: not released
        retired.retain(|s| !still(s));
        retired.extend(gone);
        drop(retired);
        self.shared.paint_pending.set(true);
        let origin = (!self.origin_logged.get() && self.is_mapped())
            .then(|| self.content_origin())
            .flatten();
        if let Some((x, y)) = origin {
            self.origin_logged.set(true);
            tracing::info!("atl-platform-ui: content origin x={x} y={y}");
        }
        mirror::resync();
        Ok(())
    }

    fn on_painted<F: FnMut(Painted) + 'static>(&self, on_painted: F) {
        self.shared.on_painted.set(Box::new(on_painted));
    }

    fn capture(&self) -> Result<Capture, UiError> {
        let fail = |what: &str| UiError::Capture(what.to_owned());
        let renderer = self
            .window
            .renderer()
            .ok_or_else(|| fail("the window has no renderer yet (not realised)"))?;
        let size = self.size();
        if size.width == 0 || size.height == 0 {
            return Err(fail("the window content has no area"));
        }
        let area = graphene::Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
        let background = gsk::ColorNode::new(&gdk::RGBA::BLACK, &area);
        let mut nodes: Vec<gsk::RenderNode> = vec![background.upcast()];
        nodes.extend(self.view.node());
        let root = gsk::ContainerNode::new(&nodes);
        let texture = renderer.render_texture(&root, Some(&area));
        let mut downloader = gdk::TextureDownloader::new(&texture);
        downloader.set_format(gdk::MemoryFormat::R8g8b8a8Premultiplied);
        let (bytes, stride) = downloader.download_bytes();
        let (width, height) = (size.width as usize, size.height as usize);
        let row = width * 4;
        if stride < row || bytes.len() < stride * (height - 1) + row {
            return Err(fail(
                "the renderer returned a smaller texture than the window",
            ));
        }
        let rgba = (0..height)
            .flat_map(|y| bytes[y * stride..y * stride + row].iter().copied())
            .collect();
        Ok(Capture { size, rgba })
    }

    fn on_resize<F: FnMut(PhysSize) + 'static>(&self, on_resize: F) {
        self.shared.on_resize.set(Box::new(on_resize));
    }

    fn on_monitor<F: FnMut(Monitor) + 'static>(&self, on_monitor: F) {
        self.shared.on_monitor.set(Box::new(on_monitor));
        // a monitor entered before the callback was set is reported now
        let current = self
            .shared
            .monitor
            .borrow()
            .as_ref()
            .map(|(m, _)| m.clone());
        if let Some(m) = current {
            report_monitor(&self.shared, &m);
        }
    }
}

/// The window entered `m`: it becomes the window's monitor, reported now and at each change of its scale, mode or rate (GdkMonitor `notify::scale` / `geometry` / `refresh-rate`). The previous monitor's handlers are disconnected.
fn follow_monitor(shared: &Rc<Shared>, m: &gdk::Monitor) {
    let previous = shared.monitor.borrow_mut().take();
    if let Some((old, handlers)) = previous {
        for handler in handlers {
            old.disconnect(handler);
        }
    }
    let handlers = ["scale", "geometry", "refresh-rate"]
        .into_iter()
        .map(|property| {
            let weak = Rc::downgrade(shared);
            m.connect_notify_local(Some(property), move |m, _| {
                if let Some(shared) = weak.upgrade() {
                    report_monitor(&shared, m);
                }
            })
        })
        .collect();
    *shared.monitor.borrow_mut() = Some((m.clone(), handlers));
    report_monitor(shared, m);
}

/// Logs `m` and hands it to the window's `on_monitor` (Monitor::from_desktop: GDK's logical geometry times the fractional scale, milli-Hz; the host side converts with the same function), with the
/// rotation of its output (outputs.rs, by connector name).
fn report_monitor(shared: &Shared, m: &gdk::Monitor) {
    let g = m.geometry();
    let connector = m.connector();
    let rotation = outputs::rotation(connector.as_deref());
    tracing::info!(
        "atl-platform-ui: window on monitor {} {}x{} at {},{} logical, {} mHz, scale {}, rotation {:?}",
        connector.as_deref().unwrap_or_default(),
        g.width(),
        g.height(),
        g.x(),
        g.y(),
        m.refresh_rate(),
        m.scale(),
        rotation
    );
    let Some(mut monitor) =
        Monitor::from_desktop(g.width(), g.height(), m.refresh_rate(), m.scale())
    else {
        return;
    };
    monitor.rotation = rotation;
    shared.on_monitor.call(|f| f(monitor));
}

/// One requested tick of `window`: GDK runs the frame clock's update phase, which the compositor's frame callback paces once a frame was committed. The composer's clock does not wait for it:
/// a tick is requested only when a published scene asks for one (`FrameRequester`).
fn request_frame(window: &gtk4::Window, shared: &Rc<Shared>) {
    if shared.frame_requested.replace(true) {
        // one tick is already on its way: a second request is the same tick (Choreographer.scheduleVsyncLocked asks once per frame)
        return;
    }
    if let Some(clock) = window.frame_clock() {
        clock.request_phase(gdk::FrameClockPhase::UPDATE);
    }
    // a request from outside a mirror pass (the app's vsync request, Java on the Looper): the frame clock's source reaches the Looper only through a sync (mirror::resync)
    mirror::resync();
}

/// A window a [`GtkFrameRequester`] names: its id, then the window and its state, weakly.
type RequestTarget = (u64, glib::WeakRef<gtk4::Window>, std::rc::Weak<Shared>);

thread_local! {
    /// The windows a [`GtkFrameRequester`] may name, by id: on the GTK thread only, so the handle itself holds nothing thread-bound and drops safely on any thread.
    static REQUEST_TARGETS: RefCell<Vec<RequestTarget>> = const { RefCell::new(Vec::new()) };
}

/// [`atl_platform_ui_api::FrameRequester`] for a GtkWindow: a request is invoked on the window's main context; requests until then coalesce into one.
pub struct GtkFrameRequester {
    id: u64,
    context: glib::MainContext,
    queued: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl atl_platform_ui_api::FrameRequester for GtkFrameRequester {
    fn request_frame(&self) {
        use std::sync::atomic::Ordering;
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let (id, queued) = (self.id, std::sync::Arc::clone(&self.queued));
        self.context.invoke(move || {
            queued.store(false, Ordering::Release);
            let target = REQUEST_TARGETS.with_borrow(|targets| {
                let (_, window, shared) = targets.iter().find(|(i, _, _)| *i == id)?;
                Some((window.upgrade()?, shared.upgrade()?))
            });
            if let Some((window, shared)) = target {
                request_frame(&window, &shared);
            }
        });
    }
}

impl FrameClock for GtkWindow {
    type Requester = GtkFrameRequester;

    fn request_frame(&self) {
        request_frame(&self.window, &self.shared);
    }

    fn frame_requester(&self) -> GtkFrameRequester {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        REQUEST_TARGETS.with_borrow_mut(|targets| {
            targets.retain(|(_, w, s)| w.upgrade().is_some() && s.upgrade().is_some());
            targets.push((id, self.window.downgrade(), Rc::downgrade(&self.shared)));
        });
        GtkFrameRequester {
            id,
            context: glib::MainContext::default(),
            queued: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn on_tick<F: FnMut(FrameTick) + 'static>(&self, on_tick: F) {
        self.shared.on_tick.set(Box::new(on_tick));
    }

    fn on_presented<F: FnMut(PresentTiming) + 'static>(&self, on_presented: F) {
        self.shared.on_presented.set(Box::new(on_presented));
    }
}

impl InputSource for GtkWindow {
    fn on_events<F: FnMut(InputEvent) + 'static>(&self, on_event: F) {
        self.shared.on_event.set(Box::new(on_event));
    }
}
