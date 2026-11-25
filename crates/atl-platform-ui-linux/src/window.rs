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
use crate::outputs;
use crate::paint_stats::PaintStats;
use crate::presentation::Presentation;
use crate::scene;
use crate::sideband;
use crate::text_input::GtkTextInput;
use crate::ui::GtkUi;
use crate::view::SceneView;

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
    /// When the pending frame request was made (CLOCK_MONOTONIC ns); None: no request pending.
    frame_requested: Cell<Option<i64>>,
    on_event: Slot<dyn FnMut(InputEvent)>,
    on_resize: Slot<dyn FnMut(PhysSize)>,
    /// The content's last allocated size (physical px), None before the first layout: `on_resize` set after it reports it (the window opens and lays out at the compositor's first configure before the session wires it).
    size: Cell<Option<PhysSize>>,
    on_monitor: Slot<dyn FnMut(Monitor)>,
    on_close_requested: Slot<dyn FnMut()>,
    /// The monitor entered last, with the handlers of its change notifications (disconnected when another is entered).
    monitor: RefCell<Option<(gdk::Monitor, Vec<glib::SignalHandlerId>)>>,
    /// The window surface's scale (GdkSurface `scale`: wp_fractional_scale_v1's preferred scale once it arrived), None before realise: the monitor report's density scale ([`reported_monitor`]).
    surface_scale: Cell<Option<f64>>,
    /// The pointer's last position over the content (logical pixels): GTK's leave and scroll signals carry none.
    pointer_at: Cell<(f64, f64)>,
    /// Time (ms) of the last pointer event published: moves must come strictly after it (`input::motion_samples`).
    pointer_time: Cell<Option<u32>>,
    /// Toolkit event times -> CLOCK_MONOTONIC: one compositor clock per seat, and a window sees one seat.
    clock: RefCell<input::ClockMap>,
    /// A touchpad swipe is open: its end (GTK `scroll-end`) is reported; a wheel's is not.
    swiping: Cell<bool>,
    /// Contact ids of the touch sequences down.
    touch_ids: RefCell<input::TouchIds>,
    on_painted: Slot<dyn FnMut(Painted)>,
    on_presented: Slot<dyn FnMut(PresentTiming)>,
    /// The window's wp_presentation feedback, bound at realise; None before, or where there is none (said once at realise).
    presentation: RefCell<Option<Presentation>>,
    /// The buffers the presented scene samples.
    shown: RefCell<Vec<scene::Shown>>,
    /// Buffers a present replaced: GSK may still draw them until the next paint; released at its after-paint.
    retired: RefCell<Vec<scene::Shown>>,
    /// A present waits for its paint: the next after-paint reports.
    paint_pending: Cell<bool>,
    /// The paint cost counters (one line a second).
    stats: RefCell<PaintStats>,
    /// This frame-clock cycle's before-paint end and paint end (ns), for `stats`.
    paint_start: Cell<Option<i64>>,
    paint_end: Cell<Option<i64>>,
}

impl Shared {
    /// Learns the toolkit clock from a `kind` event of toolkit time `t` received now; then maps `samples` (that event's own time included) onto CLOCK_MONOTONIC ns, oldest first.
    fn monotonic(&self, kind: &str, t: u32, samples: impl IntoIterator<Item = u32>) -> Vec<i64> {
        let now = monotonic_ns();
        let mut clock = self.clock.borrow_mut();
        if t == input::UNTIMED {
            tracing::debug!(
                "atl-platform-ui: input clock: untimed {kind} event, stamped at receipt"
            );
        } else if let Some(offset) = clock.learn(t, now) {
            // once per clock base: headless shares the kernel's clock (offset ~0), WSLg's Weston does not
            tracing::info!(
                "atl-platform-ui: input clock: monotonic = toolkit {offset:+} ms (learned from {kind})"
            );
        }
        samples.into_iter().map(|s| clock.map(s, now)).collect()
    }

    /// `t` (toolkit ms) of a single `kind` event on CLOCK_MONOTONIC ns.
    fn stamp(&self, kind: &str, t: u32) -> i64 {
        self.monotonic(kind, t, [t])[0]
    }

    /// After-paint of a frame that followed a present: the buffers that frame no longer shows go back, each with the reservation fences of its dma-buf as release fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` for a write: every pending GPU read of it, by this paint or an earlier one; design §3.4).
    /// A driver that attaches no implicit fences (WSL: GSK on llvmpipe, which finishes rendering before the swap) yields an already-signalled fence.
    /// The first of them with the window on a monitor grants the opening size ([`SceneView::grant_opening`]): a compositor sends the preferred fractional scale with the surface's enter, so it precedes that paint. The window keeps its logical size, the user's or the compositor's, at every scale (AOSP keeps a window's dp size across displays).
    fn painted(&self, view: Option<&SceneView>) {
        if !self.paint_pending.replace(false) {
            return;
        }
        if self.monitor.borrow().is_some() && view.is_some_and(SceneView::grant_opening) {
            tracing::info!("atl-platform-ui: opening size granted");
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

    /// Reports the presentation samples that came since the last report, in order (presentation.rs); their feedback totals go on the paint line.
    fn report_presented(&self) {
        let Some((samples, (presented, discarded))) = self
            .presentation
            .borrow_mut()
            .as_mut()
            .map(Presentation::take)
        else {
            return;
        };
        self.stats.borrow_mut().feedback(presented, discarded);
        for sample in samples {
            self.on_presented.call(|f| f(sample));
        }
    }

    /// One requested tick at `time_us`: the request is spent and the callback runs. No request, no tick: the clock is on demand.
    fn tick(&self, time_us: i64) {
        if let Some(at) = self.frame_requested.take() {
            self.stats.borrow_mut().requested(monotonic_ns() - at);
        }
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
    /// The DRM device every presented buffer must come from (GtkUi's).
    buffer_device: Option<atl_platform_ui_api::DeviceNumber>,
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

    /// Ticks, paint-cost counters and after-paint reports on the window's frame clock. Connected at realise, after GDK's own surface handlers, so our `paint` handler runs after the render.
    fn wire_frame_clock(window: &gtk4::Window, view: &SceneView, shared: &Rc<Shared>) {
        let shared = Rc::clone(shared);
        let view = view.downgrade();
        window.connect_realize(move |window| {
            let Some(clock) = window.frame_clock() else {
                tracing::warn!("realised window has no frame clock");
                return;
            };
            if let Some(renderer) = window.renderer() {
                tracing::info!("atl-platform-ui: renderer {}", renderer.type_().name());
            }
            match window.surface().map(|s| Presentation::bind(&s)) {
                Some(Ok(p)) => *shared.presentation.borrow_mut() = Some(p),
                Some(Err(why)) => {
                    tracing::info!("atl-platform-ui: no presentation samples: {why}");
                }
                None => tracing::info!("atl-platform-ui: no presentation samples: no surface"),
            }
            let on_before_paint = Rc::clone(&shared);
            clock.connect_before_paint(move |_| {
                on_before_paint.paint_start.set(Some(monotonic_ns()));
                on_before_paint.paint_end.set(None);
                if let Some(p) = on_before_paint.presentation.borrow_mut().as_mut() {
                    p.request();
                }
            });
            let on_paint = Rc::clone(&shared);
            let painted_view = view.clone();
            clock.connect_paint(move |_| {
                let now = monotonic_ns();
                on_paint.paint_end.set(Some(now));
                let snapshot = painted_view.upgrade().and_then(|v| v.take_snapshot_ns());
                if let Some(start) = on_paint.paint_start.get() {
                    let stale = !on_paint.paint_pending.get();
                    on_paint
                        .stats
                        .borrow_mut()
                        .painted(now, now - start, snapshot, stale);
                }
            });
            let on_update = Rc::clone(&shared);
            clock.connect_update(move |clock| {
                if on_update.frame_requested.get().is_some() {
                    on_update.tick(clock.frame_time());
                }
            });
            let on_after_paint = Rc::clone(&shared);
            let granted_view = view.clone();
            clock.connect_after_paint(move |_| {
                on_after_paint.painted(granted_view.upgrade().as_ref());
                on_after_paint.report_presented();
                let now = monotonic_ns();
                let after = on_after_paint.paint_end.take().map(|end| now - end);
                on_after_paint.stats.borrow_mut().cycle_done(now, after);
            });
            // a frame asked for before the window had a clock
            if shared.frame_requested.get().is_some() {
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
            let t = controller.current_event_time();
            let at = shared.stamp(&format!("pointer {phase:?}"), t);
            emit_at(shared, controller, phase, (t, x, y), at);
        }
        /// `sample` in toolkit time (kept for `motion_samples`), published at `monotonic_ns`.
        fn emit_at(
            shared: &Shared,
            controller: &impl IsA<gtk4::EventController>,
            phase: PointerPhase,
            (time_ms, x, y): input::Sample,
            monotonic_ns: i64,
        ) {
            shared.pointer_at.set((x, y));
            if time_ms != input::UNTIMED {
                shared.pointer_time.set(Some(time_ms));
            }
            let event = input::pointer(phase, x, y, scale_of(controller), monotonic_ns);
            sideband::publish(controller.current_event(), monotonic_ns, || {
                shared.on_event.call(|f| f(event))
            });
        }
        // Every pointer controller runs in the capture phase, so it publishes an event before a sideband widget's gate holds it (sideband.rs); a held event put back is Android's already and skipped.
        // raw wl_pointer buttons, not GestureClick: a click gesture interprets presses (gtk-dnd-drag-threshold -> "stopped", n_press counting), and a drag past it lost its release -> the mouse finger never lifted
        let buttons = gtk4::EventControllerLegacy::new();
        buttons.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let s = Rc::clone(shared);
        buttons.connect_event(move |c, event| {
            if sideband::replayed(Some(event)) {
                return glib::Propagation::Proceed;
            }
            let Some(b) = event.downcast_ref::<gdk::ButtonEvent>() else {
                return glib::Propagation::Proceed;
            };
            let button = input::button(b.button());
            let phase = match event.event_type() {
                gdk::EventType::ButtonPress => PointerPhase::Down(button),
                gdk::EventType::ButtonRelease => PointerPhase::Up(button),
                _ => return glib::Propagation::Proceed,
            };
            if let Some((x, y)) = view_point(c, event) {
                emit(&s, c, phase, x, y);
            }
            glib::Propagation::Proceed
        });
        view.add_controller(buttons);
        let motion = gtk4::EventControllerMotion::new();
        motion.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let s = Rc::clone(shared);
        motion.connect_motion(move |c, x, y| {
            if sideband::replayed(c.current_event().as_ref()) {
                return;
            }
            let current = (c.current_event_time(), x, y);
            let history = c.current_event().map(|e| e.history()).unwrap_or_default();
            let view = c.widget();
            let earlier = history.iter().filter_map(|t| {
                let axes = t.axes();
                let at = surface_to_view(view.as_ref()?, (axes[AXIS_X], axes[AXIS_Y]))?;
                Some((t.time(), at.0, at.1))
            });
            let samples = input::motion_samples(earlier, current, s.pointer_time.get());
            let stamps = s.monotonic("pointer motion", current.0, samples.iter().map(|m| m.0));
            for (sample, at) in samples.into_iter().zip(stamps) {
                emit_at(&s, c, PointerPhase::Move, sample, at);
            }
        });
        let s = Rc::clone(shared);
        motion.connect_enter(move |c, x, y| {
            if !sideband::replayed(c.current_event().as_ref()) {
                emit(&s, c, PointerPhase::Enter, x, y)
            }
        });
        let s = Rc::clone(shared);
        motion.connect_leave(move |c| {
            if sideband::replayed(c.current_event().as_ref()) {
                return;
            }
            let (x, y) = s.pointer_at.get();
            emit(&s, c, PointerPhase::Leave, x, y);
        });
        view.add_controller(motion);
    }

    /// Wheel and touchpad scrolling. Not KINETIC (Android flings itself from the swipe's own events, GestureConverter.cpp handleFling) and not DISCRETE (hi-res wheels report fractions).
    /// The unit tells a wheel from fingers; GDK gives finger scrolling its own device, so `scroll-end` marks the fingers lifting (wl_pointer.axis_stop).
    fn wire_scroll(view: &SceneView, shared: &Rc<Shared>) {
        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let s = Rc::clone(shared);
        scroll.connect_scroll(move |c, dx, dy| {
            if sideband::replayed(c.current_event().as_ref()) {
                return gtk4::glib::Propagation::Proceed;
            }
            let unit = match c.unit() {
                gdk::ScrollUnit::Wheel => input::ScrollUnit::Wheel,
                _ => input::ScrollUnit::Surface,
            };
            s.swiping.set(unit == input::ScrollUnit::Surface);
            let at = s.stamp("scroll", c.current_event_time());
            let event = input::scroll(unit, dx, dy, s.pointer_at.get(), scale_of(c), at);
            sideband::publish(c.current_event(), at, || s.on_event.call(|f| f(event)));
            // a scroll over a sideband widget goes on to its gate
            if sideband::routes_to_gate(picked(c, s.pointer_at.get())) {
                gtk4::glib::Propagation::Proceed
            } else {
                gtk4::glib::Propagation::Stop
            }
        });
        let s = Rc::clone(shared);
        scroll.connect_scroll_end(move |c| {
            if s.swiping.replace(false) {
                let at = s.stamp("scroll end", c.current_event_time());
                let event = input::swipe_end(s.pointer_at.get(), scale_of(c), at);
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
            if sideband::replayed(Some(event)) {
                return glib::Propagation::Proceed;
            }
            let phase = match event.event_type() {
                gdk::EventType::TouchBegin => TouchPhase::Down,
                gdk::EventType::TouchUpdate => TouchPhase::Move,
                gdk::EventType::TouchEnd => TouchPhase::Up,
                gdk::EventType::TouchCancel => TouchPhase::Cancel,
                _ => return glib::Propagation::Proceed,
            };
            let sequence: *mut gdk::ffi::GdkEventSequence = event.event_sequence().to_glib_none().0;
            let id = s.touch_ids.borrow_mut().id(sequence as usize, phase);
            let point = view_point(c, event);
            if let (Some(id), Some((x, y))) = (id, point) {
                let at = s.stamp("touch", c.current_event_time());
                let event = input::touch(phase, id, x, y, scale_of(c), at);
                sideband::publish(c.current_event(), at, || s.on_event.call(|f| f(event)));
            }
            // a touch on a sideband widget goes on to its gate
            if point.is_some_and(|p| sideband::routes_to_gate(picked(c, p))) {
                glib::Propagation::Proceed
            } else {
                glib::Propagation::Stop
            }
        });
        view.add_controller(legacy);
    }

    /// Keys of the toplevel (the window has keyboard focus): presses, GDK's synthesized repeats (the desktop's `repeat_info`), releases. The IME item puts its GtkIMContext on this controller, so keys it consumes never arrive here.
    /// Capture phase, so a key reaches Android before a focused sideband widget's gate holds it; a held key put back is skipped (sideband.rs).
    fn wire_keys(window: &gtk4::Window, shared: &Rc<Shared>) {
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let send = |s: &Rc<Shared>, c: &gtk4::EventControllerKey, down: bool, keycode: u32| {
            let focus = c.widget().and_then(|w| w.root()).and_then(|r| r.focus());
            let to_gate = sideband::routes_to_gate(focus.clone());
            // which widget had the keyboard focus: a key that never shows here was lost before the window (toolkit focus elsewhere)
            let held_by = match &focus {
                None => "none",
                Some(_) if to_gate => "sideband",
                Some(w) if w.is::<SceneView>() => "view",
                Some(_) => "other",
            };
            crate::count::hit(&format!("key.received.focus-{held_by}"));
            if sideband::replayed(c.current_event().as_ref()) {
                crate::count::hit("key.replay-passed");
                return glib::Propagation::Proceed;
            }
            // a page's unhandled press, put back by WebKit: reported to Android as unhandled, not published a second time
            if sideband::unhandled_key(c.current_event().as_ref()) {
                crate::count::hit("key.unhandled-reported");
                return glib::Propagation::Stop;
            }
            let locks = c
                .current_event_device()
                .map(|d| Locks {
                    caps: d.is_caps_locked(),
                    num: d.is_num_locked(),
                    scroll: d.is_scroll_locked(),
                })
                .unwrap_or_default();
            let at = s.stamp("key", c.current_event_time());
            if let Some(event) = input::key(down, keycode, locks, at) {
                crate::count::hit("key.published");
                sideband::publish(c.current_event(), at, || s.on_event.call(|f| f(event)));
            } else {
                crate::count::hit("key.no-scancode");
            }
            if to_gate {
                glib::Propagation::Proceed
            } else {
                glib::Propagation::Stop
            }
        };
        let s = Rc::clone(shared);
        keys.connect_key_pressed(move |c, _, keycode, _| send(&s, c, true, keycode));
        let s = Rc::clone(shared);
        keys.connect_key_released(move |c, _, keycode, _| {
            send(&s, c, false, keycode);
        });
        window.add_controller(keys);
    }
}

/// What GTK may size an opening window to, from the bounds GDK gives it. A compositor that sends no `configure_bounds` leaves GDK to use the union of its monitors, whose logical sizes are wrong while a monitor's scale is a guess
/// (1440x900 at 2 for a 2880x1800 output at 1.6, really 1800x1125): those are recomputed at the opening scale. None: `bounds` are not that union, so they are the compositor's and stand.
fn opening_bounds(
    bounds: (i32, i32),
    monitors: &[(gdk::Rectangle, f64)],
    opening: Scale,
) -> Option<(i32, i32)> {
    let union = monitors
        .iter()
        .map(|(g, _)| *g)
        .reduce(|a, b| a.union(&b))?;
    if (union.width(), union.height()) != bounds {
        return None;
    }
    monitors
        .iter()
        .map(|(g, scale)| {
            let real = |v: i32| opening.to_logical(f64::from(v) * scale).round() as i32;
            gdk::Rectangle::new(real(g.x()), real(g.y()), real(g.width()), real(g.height()))
        })
        .reduce(|a, b| a.union(&b))
        .map(|r| (r.width(), r.height()))
}

/// A window layout's fixed parts, logical pixels (width, height): the chrome around the content inside the window, and the surface around the window (the client-side shadow).
#[derive(Clone, Copy)]
struct Frame {
    chrome: (i32, i32),
    shadow: (i32, i32),
}

/// Keeps GTK's clamp to phony bounds ([`opening_bounds`]) from cutting the opening size. GTK exposes neither the shadow nor the chrome before a layout, so the first (clamped) layout gives them and asks for a second in the same frame, before anything is drawn.
/// Connected at realise: GTK's own handlers run first.
fn follow_opening_bounds(
    toplevel: &gdk::Toplevel,
    window: &gtk4::Window,
    view: &glib::WeakRef<SceneView>,
) {
    let frame: Rc<Cell<Option<Frame>>> = Rc::default();
    let (measured, sized_view) = (Rc::clone(&frame), view.clone());
    toplevel.connect_compute_size(move |toplevel, size| {
        let (Some(frame), Some(view)) = (measured.get(), sized_view.upgrade()) else {
            return;
        };
        let Some(opening) = view.opening() else {
            return;
        };
        let bounds = size.bounds();
        let wanted = view.wanted();
        let full = (wanted.0 + frame.chrome.0, wanted.1 + frame.chrome.1);
        if full.0 <= bounds.0 && full.1 <= bounds.1 {
            return;
        }
        let monitors: Vec<_> = toplevel
            .display()
            .monitors()
            .iter::<gdk::Monitor>()
            .flatten()
            .map(|m| (m.geometry(), m.scale()))
            .collect();
        if let Some(limit) = opening_bounds(bounds, &monitors, opening) {
            size.set_size(
                full.0.min(limit.0) + frame.shadow.0,
                full.1.min(limit.1) + frame.shadow.1,
            );
        }
    });
    let (window, view) = (window.downgrade(), view.clone());
    toplevel.connect_layout(move |surface, width, height| {
        let (Some(window), Some(view)) = (window.upgrade(), view.upgrade()) else {
            return;
        };
        if frame.get().is_some() || view.opening().is_none() || view.width() <= 0 {
            return;
        }
        frame.set(Some(Frame {
            chrome: (
                window.width() - view.width(),
                window.height() - view.height(),
            ),
            shadow: (width - window.width(), height - window.height()),
        }));
        let wanted = view.wanted();
        if view.width() < wanted.0 || view.height() < wanted.1 {
            surface.request_layout();
        }
    });
}

/// Sizes the window to the content's wanted logical size, keeping the chrome around the content. Runs at each change of the wanted size.
fn fit_window(window: &gtk4::Window, view: &SceneView) {
    if !(window.is_mapped() && window.surface().is_some_and(|s| s.is_mapped())) {
        return;
    }
    let (width, height) = view.wanted();
    // the window is the content plus the chrome around it: keep the chrome, change the content
    let (chrome_w, chrome_h) = (
        window.width() - view.width(),
        window.height() - view.height(),
    );
    tracing::info!(
        "atl-platform-ui: window fit to {}x{} logical, content {width}x{height} at scale {}",
        width + chrome_w,
        height + chrome_h,
        view.scale().get(),
    );
    window.set_default_size(width + chrome_w, height + chrome_h);
}

/// A raw event's position in the view: the event's is the surface's, the content's is past the shadow and the header bar.
fn view_point(c: &gtk4::EventControllerLegacy, event: &gdk::Event) -> Option<(f64, f64)> {
    surface_to_view(&c.widget()?, event.position()?)
}

/// `GdkTimeCoord.axes` indices of the position: the array is indexed by `GdkAxisUse` (`GDK_AXIS_X` 1, `GDK_AXIS_Y` 2).
const AXIS_X: usize = 1;
const AXIS_Y: usize = 2;

/// A surface position (an event's, a history sample's) in `view`: past the shadow and the header bar.
fn surface_to_view(view: &gtk4::Widget, (x, y): (f64, f64)) -> Option<(f64, f64)> {
    let native = view.native()?;
    let (shadow_x, shadow_y) = native.surface_transform();
    let at = native.compute_point(
        view,
        &graphene::Point::new((x - shadow_x) as f32, (y - shadow_y) as f32),
    )?;
    Some((f64::from(at.x()), f64::from(at.y())))
}

/// The widget under `(x, y)` of the controller's view (logical pixels).
fn picked(c: &impl IsA<gtk4::EventController>, (x, y): (f64, f64)) -> Option<gtk4::Widget> {
    c.widget()?.pick(x, y, gtk4::PickFlags::DEFAULT)
}

/// CLOCK_MONOTONIC now, ns: the base of published input times (`input::ClockMap`).
pub(crate) fn monotonic_ns() -> i64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec * 1_000_000_000 + now.tv_nsec
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
            frame_requested: Cell::new(None),
            on_event: Slot::new(),
            on_resize: Slot::new(),
            size: Cell::new(None),
            pointer_at: Cell::new((0.0, 0.0)),
            pointer_time: Cell::new(None),
            clock: RefCell::default(),
            swiping: Cell::new(false),
            touch_ids: RefCell::default(),
            on_monitor: Slot::new(),
            on_close_requested: Slot::new(),
            monitor: RefCell::new(None),
            surface_scale: Cell::new(None),
            on_painted: Slot::new(),
            on_presented: Slot::new(),
            presentation: RefCell::new(None),
            shown: RefCell::default(),
            retired: RefCell::default(),
            paint_pending: Cell::new(false),
            stats: RefCell::default(),
            paint_start: Cell::new(None),
            paint_end: Cell::new(None),
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
        Self::wire_frame_clock(&window, &view, &shared);
        Self::wire_pointer(&view, &shared);
        Self::wire_scroll(&view, &shared);
        Self::wire_touch(&view, &shared);
        Self::wire_keys(&window, &shared);
        // the monitor the window is on (wl_surface.enter): reported at each entry and at each change of that monitor (Window::on_monitor)
        // and its scale (wp_fractional_scale_v1 preferred_scale arrives after realisation): the content keeps its logical size and is reported in the new physical pixels
        let entered = Rc::clone(&shared);
        let opening_view = view.downgrade();
        let scaled = view.downgrade();
        window.connect_realize(move |w| {
            if let Some(surface) = w.surface() {
                entered.surface_scale.set(Some(surface.scale()));
                let rescaled = Rc::downgrade(&entered);
                let entered = Rc::clone(&entered);
                surface.connect_enter_monitor(move |_, m| follow_monitor(&entered, m));
                let scaled = scaled.clone();
                surface.connect_scale_notify(move |s| {
                    tracing::info!("atl-platform-ui: surface scale {}", s.scale());
                    // the density follows the surface's scale: the monitor is reported again at it
                    if let Some(shared) = rescaled.upgrade() {
                        shared.surface_scale.set(Some(s.scale()));
                        let monitor = shared.monitor.borrow().as_ref().map(|(m, _)| m.clone());
                        if let Some(m) = monitor {
                            report_monitor(&shared, &m);
                        }
                    }
                    // from the next idle, not inside the notification: GTK dropped a size request made there
                    let scaled = scaled.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(view) = scaled.upgrade() {
                            view.rescale();
                        }
                    });
                });
                // the xdg-shell states GDK reports: `suspended` is the hidden signal the window lifecycle consumes (S2); logged per change
                if let Ok(toplevel) = surface.downcast::<gdk::Toplevel>() {
                    follow_opening_bounds(&toplevel, w, &opening_view);
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
        // the desktop's close (xdg_toplevel.close): reported, and the window stays: Android removes the Task and the window goes with its process (Window::on_close_requested)
        let closing = Rc::clone(&shared);
        window.connect_close_request(move |_| {
            tracing::info!("atl-platform-ui: close requested");
            closing.on_close_requested.call(|f| f());
            glib::Propagation::Stop
        });
        let resized = Rc::clone(&shared);
        view.connect_allocation(move |size| {
            resized.size.set(Some(size));
            resized.on_resize.call(|f| f(size));
        });
        window.set_child(Some(&view));
        let text_input = GtkTextInput::new(&view);
        let this = GtkWindow {
            _output_follow: output_follow,
            buffer_device: ui.buffer_device,
            window,
            view,
            requested: Cell::new(attributes.size),
            shared,
            origin_logged: Cell::new(false),
            text_input,
        };
        this.set_title(&attributes.title);
        this.view.open_at(attributes.scale);
        this.set_size(attributes.size);
        this.window.present();
        Ok(this)
    }

    fn set_size(&self, size: PhysSize) {
        self.requested.set(size);
        self.view.set_wanted(size);
        fit_window(&self.window, &self.view);
    }

    fn set_title(&self, title: &str) {
        self.window.set_title(Some(title));
    }

    /// On the content view: GDK sets it through `wp_cursor_shape_v1` where the compositor has it, else a themed cursor surface.
    fn set_cursor(&self, shape: CursorShape) {
        let name = input::cursor_name(shape);
        tracing::info!("atl-platform-ui: cursor {name}");
        self.view.set_cursor_from_name(Some(name));
    }

    /// `gtk_window_minimize`: GDK's Wayland backend sends `xdg_toplevel.set_minimized`.
    fn minimize(&self) {
        tracing::info!("atl-platform-ui: minimize");
        self.window.minimize();
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
        let start = monotonic_ns();
        let (pieces, shown) = scene::build(
            &WidgetExt::display(&self.window),
            scene,
            self.buffer_device,
            &self.shared.shown.borrow(),
        )?;
        let reused = shown
            .iter()
            .filter(|s| {
                let previous = self.shared.shown.borrow();
                previous.iter().any(|p| p.texture == s.texture)
            })
            .count() as u64;
        self.shared.stats.borrow_mut().presented(
            monotonic_ns() - start,
            shown.len() as u64 - reused,
            reused,
        );
        self.view.show_pieces(pieces);
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
            // with the scale: a content pixel p is the surface point origin + p / scale
            tracing::info!(
                "atl-platform-ui: content origin x={x} y={y} scale={}",
                self.view.scale().get()
            );
        }
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
        // the widget's own picture, so sideband children (WebKit) are in it
        nodes.extend(self.view.picture());
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
        // a size allocated before the callback was set is reported now
        if let Some(size) = self.shared.size.get() {
            self.shared.on_resize.call(|f| f(size));
        }
    }

    fn on_close_requested<F: FnMut() + 'static>(&self, on_close_requested: F) {
        self.shared
            .on_close_requested
            .set(Box::new(on_close_requested));
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

/// Logs `m` and hands it to the window's `on_monitor` ([`reported_monitor`]), with the rotation of its output (outputs.rs, by connector name).
fn report_monitor(shared: &Shared, m: &gdk::Monitor) {
    let g = m.geometry();
    let connector = m.connector();
    let rotation = outputs::rotation(connector.as_deref());
    let surface_scale = shared.surface_scale.get();
    tracing::info!(
        "atl-platform-ui: window on monitor {} {}x{} at {},{} logical, {} mHz, scale {}, surface scale {:?}, rotation {:?}",
        connector.as_deref().unwrap_or_default(),
        g.width(),
        g.height(),
        g.x(),
        g.y(),
        m.refresh_rate(),
        m.scale(),
        surface_scale,
        rotation
    );
    let Some(mut monitor) = reported_monitor(
        g.width(),
        g.height(),
        m.refresh_rate(),
        m.scale(),
        surface_scale,
    ) else {
        return;
    };
    monitor.rotation = rotation;
    shared.on_monitor.call(|f| f(monitor));
}

/// The monitor a window reports, from GDK's monitor (logical size, milli-Hz, scale) and the window surface's scale. Size: logical x the monitor's scale (Monitor::from_desktop; the host side converts
/// with the same function) = the mode, with or without xdg-output. Scale (-> density): the surface's fractional scale when it has one ([`crate::view::output_scale`]), else the monitor's.
/// Why not the monitor's alone: without zxdg_output_manager_v1 (Hyprland withholds it from security-context clients, which every app process is) GDK knows only wl_output's integer scale, 2 on a
/// 1.6 output, and its logical size is the mode / 2; the density then disagreed with atld's own (unsandboxed) reading of the same monitor, and the app saw a density change at start.
fn reported_monitor(
    logical_width: i32,
    logical_height: i32,
    millihz: i32,
    monitor_scale: f64,
    surface_scale: Option<f64>,
) -> Option<Monitor> {
    let mut monitor = Monitor::from_desktop(logical_width, logical_height, millihz, monitor_scale)?;
    monitor.scale = Scale::new(crate::view::output_scale(
        surface_scale,
        Some(monitor_scale),
        monitor_scale,
    ))?;
    Some(monitor)
}

/// One requested tick of `window`: GDK runs the frame clock's update phase, which the compositor's frame callback paces once a frame was committed. The composer's clock does not wait for it:
/// a tick is requested only when a published scene asks for one (`FrameRequester`).
fn request_frame(window: &gtk4::Window, shared: &Rc<Shared>) {
    if shared.frame_requested.get().is_some() {
        // one tick is already on its way: a second request is the same tick (Choreographer.scheduleVsyncLocked asks once per frame)
        return;
    }
    shared.frame_requested.set(Some(monotonic_ns()));
    if let Some(clock) = window.frame_clock() {
        clock.request_phase(gdk::FrameClockPhase::UPDATE);
    }
}

/// A window a [`GtkFrameRequester`] names: its id, then the window and its state, weakly.
type RequestTarget = (u64, glib::WeakRef<gtk4::Window>, std::rc::Weak<Shared>);

thread_local! {
    /// The windows a [`GtkFrameRequester`] may name, by id: on the display thread only, so the handle itself holds nothing thread-bound and drops safely on any thread.
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

#[cfg(test)]
mod tests {
    use super::{opening_bounds, reported_monitor};
    use atl_platform_ui_api::Scale;
    use gtk4::gdk;

    /// The laptop as GDK guesses it: 1440 x 2 / 1.6 = 1800, 900 x 2 / 1.6 = 1125. Any other bounds are the compositor's. A second 1440x900 guess to the right doubles the width.
    #[test]
    fn bounds_of_guessed_monitors_are_taken_at_the_opening_scale() {
        let scale = Scale::new(1.6).unwrap();
        let one = [(gdk::Rectangle::new(0, 0, 1440, 900), 2.0)];
        assert_eq!(opening_bounds((1440, 900), &one, scale), Some((1800, 1125)));
        assert_eq!(opening_bounds((1440, 860), &one, scale), None);
        assert_eq!(opening_bounds((1440, 900), &[], scale), None);
        let two = [one[0], (gdk::Rectangle::new(1440, 0, 1440, 900), 2.0)];
        assert_eq!(opening_bounds((2880, 900), &two, scale), Some((3600, 1125)));
        // a monitor whose scale is already the opening's stays
        let exact = [(gdk::Rectangle::new(0, 0, 1800, 1125), 1.6)];
        assert_eq!(
            opening_bounds((1800, 1125), &exact, scale),
            Some((1800, 1125))
        );
    }

    /// The laptop (2880 x 1800 panel at Hyprland scale 1.6), measured 2026-11-22. Inside a security context GDK said 1440 x 900 at scale 2 (no xdg-output: mode / integer scale) while the
    /// surface's fractional scale was 1.6; atld's own connection said 1800 x 1125 at 1.6. Both must give the mode and scale 1.6 (density 160 x 1.6 = 256, not 160 x 2 = 320).
    #[test]
    fn no_xdg_output_reports_the_fractional_scale() {
        let sandboxed = reported_monitor(1440, 900, 120_000, 2.0, Some(1.6)).unwrap();
        let host = reported_monitor(1800, 1125, 120_000, 1.6, Some(1.6)).unwrap();
        for m in [sandboxed, host] {
            assert_eq!((m.size.width, m.size.height), (2880, 1800));
            assert_eq!(m.scale.get(), 1.6);
        }
    }

    /// Before the fractional scale arrives (or on an integer output) the surface's scale is an integer: the monitor's scale stands.
    #[test]
    fn an_integer_surface_scale_leaves_the_monitor_scale() {
        let m = reported_monitor(1920, 1200, 60_000, 1.5, Some(2.0)).unwrap();
        assert_eq!(m.scale.get(), 1.5);
        let m = reported_monitor(1440, 900, 60_000, 2.0, None).unwrap();
        assert_eq!(
            (m.size.width, m.size.height, m.scale.get()),
            (2880, 1800, 2.0)
        );
    }
}
