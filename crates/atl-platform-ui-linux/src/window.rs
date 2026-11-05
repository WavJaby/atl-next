// SPDX-License-Identifier: MIT
//! `GtkWindow`: a `GtkWindow` whose only content is the `SceneView`, with its GdkFrameClock as the `FrameClock` and pointer controllers as the `InputSource`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use atl_platform_ui_api::{
    Capture, FrameClock, FrameTick, InputEvent, InputSource, PhysSize, PointerPhase, PresentError,
    Scale, Scene, UiError, Window, WindowAttributes,
};
use gtk4::prelude::*;
use gtk4::{gdk, graphene, gsk};

use crate::input;
use crate::scene;
use crate::ui::GtkUi;
use crate::view::SceneView;

/// GDK frame times are microseconds (`g_get_monotonic_time`), ticks are nanoseconds.
const NS_PER_US: u64 = 1000;

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
                if on_update.frame_requested.replace(false) {
                    let time_ns = u64::try_from(clock.frame_time()).unwrap_or(0) * NS_PER_US;
                    on_update.on_tick.call(|f| f(FrameTick { time_ns }));
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
            let scale = controller
                .widget()
                .and_then(|w| w.downcast::<SceneView>().ok())
                .map_or(Scale::ONE, |v| v.scale());
            let event = input::pointer(phase, x, y, scale, controller.current_event_time());
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
        view.add_controller(motion);
    }
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
        });
        let window = gtk4::Window::new();
        let view = SceneView::new();
        Self::wire_frame_clock(&window, &shared);
        Self::wire_pointer(&view, &shared);
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
    }

    fn set_title(&self, title: &str) {
        self.window.set_title(Some(title));
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
        let origin = (!self.origin_logged.get() && self.is_mapped())
            .then(|| self.content_origin())
            .flatten();
        if let Some((x, y)) = origin {
            self.origin_logged.set(true);
            tracing::info!("atl-platform-ui: content origin x={x} y={y}");
        }
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
}

impl FrameClock for GtkWindow {
    fn request_frame(&self) {
        self.shared.frame_requested.set(true);
        if let Some(clock) = self.window.frame_clock() {
            clock.request_phase(gdk::FrameClockPhase::UPDATE);
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
