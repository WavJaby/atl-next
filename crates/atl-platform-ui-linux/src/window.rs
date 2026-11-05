// SPDX-License-Identifier: MIT
//! `GtkWindow`: a `GtkWindow` whose only content is the `SceneView`, with its GdkFrameClock as the `FrameClock` and pointer controllers as the `InputSource`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use atl_platform_ui_api::{
    Capture, FrameClock, FrameTick, InputEvent, InputSource, Monitor, PhysSize, PointerPhase,
    PresentError, Scale, Scene, UiError, Window, WindowAttributes,
};
use gtk4::prelude::*;
use gtk4::{gdk, glib, graphene, gsk};

use crate::input;
use crate::mirror;
use crate::pacing;
use crate::scene;
use crate::ui::GtkUi;
use crate::view::SceneView;

/// GDK frame times are microseconds (`g_get_monotonic_time`), ticks are nanoseconds.
const NS_PER_US: u64 = 1000;
/// GDK's frame clock default when no refresh rate is known (60 Hz, in GDK's milli-Hz).
const DEFAULT_REFRESH_MILLIHZ: i32 = 60_000;

/// A callback slot that tolerates the callback replacing itself while it runs: it is taken out for the call and put back unless a new one was set.
struct Slot<T: ?Sized>(RefCell<Option<Box<T>>>);

impl<T: ?Sized> Slot<T> {
    fn new() -> Self {
        Slot(RefCell::new(None))
    }

    fn set(&self, f: Box<T>) {
        *self.0.borrow_mut() = Some(f);
    }

    fn call(&self, run: impl FnOnce(&mut T)) {
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
    /// The current tick presented a scene: the next is paced by the compositor's frame callback, else by the idle timer (pacing.rs).
    presented: Cell<bool>,
    /// The last tick's time, µs CLOCK_MONOTONIC: the idle grid's origin.
    last_tick_us: Cell<Option<i64>>,
    /// The pending idle tick.
    idle_timer: RefCell<Option<glib::SourceId>>,
    stats: RefCell<pacing::TickStats>,
    /// The pointer's last position over the content (logical pixels): GTK's leave and scroll signals carry none.
    pointer_at: Cell<(f64, f64)>,
    /// A touchpad swipe is open: its end (GTK `scroll-end`) is reported; a wheel's is not.
    swiping: Cell<bool>,
}

impl Shared {
    /// One requested tick at `time_us`: the request is spent, a pending timer tick is dropped (this one replaces it), the tick is counted, and the callback runs. No request, no tick: the clock is on demand.
    fn tick(&self, time_us: i64, idle: bool) {
        self.frame_requested.set(false);
        if let Some(id) = self.idle_timer.borrow_mut().take() {
            id.remove();
        }
        self.last_tick_us.set(Some(time_us));
        let report = self.stats.borrow_mut().count(idle, time_us);
        if let Some((paced, idle_ticks, elapsed_us)) = report {
            tracing::info!(
                "atl-platform-ui: frame clock paced={paced} idle={idle_ticks} over_us={elapsed_us}"
            );
        }
        let time_ns = u64::try_from(time_us).unwrap_or(0) * NS_PER_US;
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
}

impl GtkWindow {
    fn logical(&self, physical: u32) -> i32 {
        self.view.scale().to_logical(f64::from(physical)).ceil() as i32
    }

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
                    on_update.tick(clock.frame_time(), false);
                }
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

    fn create(ui: &GtkUi, attributes: &WindowAttributes) -> Result<Self, UiError> {
        ui.check_thread()?;
        let shared = Rc::new(Shared {
            on_tick: Slot::new(),
            frame_requested: Cell::new(false),
            on_event: Slot::new(),
            on_resize: Slot::new(),
            pointer_at: Cell::new((0.0, 0.0)),
            swiping: Cell::new(false),
            on_monitor: Slot::new(),
            monitor: RefCell::new(None),
            presented: Cell::new(false),
            last_tick_us: Cell::new(None),
            idle_timer: RefCell::new(None),
            stats: RefCell::new(pacing::TickStats::default()),
        });
        let window = gtk4::Window::new();
        let view = SceneView::new();
        Self::wire_frame_clock(&window, &shared);
        Self::wire_pointer(&view, &shared);
        Self::wire_scroll(&view, &shared);
        // the monitor the window is on (wl_surface.enter): reported at each entry and at each change of that monitor (Window::on_monitor); the idle pacing reads its rate (monitor_interval_us)
        let entered = Rc::clone(&shared);
        window.connect_realize(move |w| {
            if let Some(surface) = w.surface() {
                let entered = Rc::clone(&entered);
                surface.connect_enter_monitor(move |_, m| follow_monitor(&entered, m));
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
        let this = GtkWindow {
            window,
            view,
            requested: Cell::new(attributes.size),
            shared,
            origin_logged: Cell::new(false),
        };
        this.set_title(&attributes.title);
        this.set_size(attributes.size);
        this.window.present();
        mirror::resync();
        Ok(this)
    }

    fn set_size(&self, size: PhysSize) {
        self.requested.set(size);
        let (width, height) = (self.logical(size.width), self.logical(size.height));
        self.view.set_wanted(width, height);
        if self.is_mapped() {
            // the window is the content plus the chrome around it: keep the chrome, change the content
            let (chrome_w, chrome_h) = (
                self.window.width() - self.view.width(),
                self.window.height() - self.view.height(),
            );
            self.window
                .set_default_size(width + chrome_w, height + chrome_h);
        }
        // every GTK change this layer makes may attach a source (a layout or paint request of the frame clock): from outside a mirror pass it reaches the Looper only through a sync. The platform layer is the only GTK caller, so its entry points are the complete set.
        mirror::resync();
    }

    fn set_title(&self, title: &str) {
        self.window.set_title(Some(title));
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

    fn present(&self, scene: &Scene) -> Result<(), PresentError> {
        self.view.show_node(scene::build(scene)?);
        self.shared.presented.set(true);
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

/// Logs `m` and hands it to the window's `on_monitor` (Monitor::from_desktop: GDK's logical geometry times the fractional scale, milli-Hz; the host side converts with the same function).
fn report_monitor(shared: &Shared, m: &gdk::Monitor) {
    let g = m.geometry();
    tracing::info!(
        "atl-platform-ui: window on monitor {} {}x{} at {},{} logical, {} mHz, scale {}",
        m.connector().map(|c| c.to_string()).unwrap_or_default(),
        g.width(),
        g.height(),
        g.x(),
        g.y(),
        m.refresh_rate(),
        m.scale()
    );
    let Some(monitor) = Monitor::from_desktop(g.width(), g.height(), m.refresh_rate(), m.scale())
    else {
        return;
    };
    shared.on_monitor.call(|f| f(monitor));
}

/// One requested tick of `window`. After a tick that presented (or before the first tick) GDK paces the next: the compositor's frame callback. Otherwise the timer ticks on the monitor's grid (pacing.rs: GDK's own fallback runs at 60 Hz).
fn request_frame(window: &gtk4::Window, shared: &Rc<Shared>) {
    if shared.frame_requested.replace(true) {
        // one tick is already on its way: a second request is the same tick (Choreographer.scheduleVsyncLocked asks once per frame)
        return;
    }
    let last = shared.last_tick_us.get();
    let Some(last) = last.filter(|_| !shared.presented.replace(false)) else {
        if let Some(clock) = window.frame_clock() {
            clock.request_phase(gdk::FrameClockPhase::UPDATE);
        }
        // the frame clock's own source, same as the timer below
        mirror::resync();
        return;
    };
    let interval = monitor_interval_us(window);
    let now = glib::monotonic_time();
    let at = pacing::next_grid_us(last, interval, now);
    // whole ms, rounded up: the timer fires at or after the grid point, never before it (a point fired early would be computed again)
    let delay_ms = u64::try_from((at - now + 999) / 1000).unwrap_or(0);
    let fired = Rc::clone(shared);
    let id = glib::timeout_add_local_once(Duration::from_millis(delay_ms), move || {
        // fired: the id is spent and must not be removed again
        fired.idle_timer.borrow_mut().take();
        if fired.frame_requested.get() {
            fired.tick(at, true);
        }
    });
    if let Some(old) = shared.idle_timer.replace(Some(id)) {
        old.remove();
    }
    // a request from outside a mirror pass (the app's vsync request, Java on the Looper): the new timeout reaches the Looper only through a sync (mirror::resync)
    mirror::resync();
}

/// The refresh interval of the monitor `window` is on; GDK's own default (60 Hz) where the monitor reports none.
fn monitor_interval_us(window: &gtk4::Window) -> i64 {
    let millihz = window
        .surface()
        .and_then(|s| WidgetExt::display(window).monitor_at_surface(&s))
        .map_or(0, |m| m.refresh_rate());
    pacing::interval_us(millihz)
        .or_else(|| pacing::interval_us(DEFAULT_REFRESH_MILLIHZ))
        .unwrap_or(1)
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
}

impl InputSource for GtkWindow {
    fn on_events<F: FnMut(InputEvent) + 'static>(&self, on_event: F) {
        self.shared.on_event.set(Box::new(on_event));
    }
}
