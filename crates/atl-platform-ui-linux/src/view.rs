// SPDX-License-Identifier: MIT
//! `SceneView`: the one widget of a window, drawing the presented scene's pieces. No per-widget GTK peers (plan S3): Android renders every pixel of the app, GTK draws only these nodes and its own chrome.
//! The one exception is a sideband quad's content (webview plan S2.2 amended): a toolkit widget registered under its stream (`sideband`; WebKit's view), made a child of the view whose scene shows it,
//! allocated at the quad's place and drawn by `snapshot_child` at the quad's depth under its scissor and alpha, so GTK's picking finds it there.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};

use atl_libgui_parcel::state::FloatRect;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, gsk};

use crate::scene::Piece;
use crate::sideband::resolve;

/// Sideband quads drawn as nothing: a tag other than `SIDEBAND_WEB_VIEW` or a key with no widget (logged on the first of each).
static SIDEBAND_UNRESOLVED: AtomicU64 = AtomicU64::new(0);

/// Sideband show/hide transitions logged with their time, per direction; later ones only counted (`count sideband.shown|hidden`). 16: a dialog opened and closed a few times, not a flapping layer.
const TRANSITIONS_LOGGED: u64 = 16;

fn logical_rect(r: &FloatRect, to_logical: f32) -> graphene::Rect {
    graphene::Rect::new(
        r.left * to_logical,
        r.top * to_logical,
        (r.right - r.left) * to_logical,
        (r.bottom - r.top) * to_logical,
    )
}

/// A content size change in physical pixels ([`SceneView::connect_allocation`]).
type AllocateFn = Box<dyn FnMut(atl_platform_ui_api::PhysSize)>;

/// Logical pixels whose physical size at `scale` is nearest `physical`. Rounded, not ceiled: 1125 at 1.6 is 703.125, and ceil's 704 maps back to 1126.
pub fn logical_px(scale: atl_platform_ui_api::Scale, physical: u32) -> i32 {
    scale.to_logical(f64::from(physical)).round() as i32
}

/// Physical pixels per logical pixel of a window's output. Until the fractional scale arrives GDK's surface scale is the integer one (2 on a 1.6 output), and a content measured at it is too
/// small: a fractional surface scale is final and wins; else the output's monitor; else the surface's integer scale; else the widget's.
pub fn output_scale(surface: Option<f64>, monitor: Option<f64>, widget: f64) -> f64 {
    match (surface, monitor) {
        (Some(s), _) if s.fract() != 0.0 => s,
        (_, Some(m)) => m,
        (Some(s), None) => s,
        (None, None) => widget,
    }
}

/// The desktop's first monitor: the output a window is measured on before its surface is on one.
fn first_monitor(display: &gdk::Display) -> Option<gdk::Monitor> {
    display.monitors().item(0)?.downcast().ok()
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SceneView {
        /// The presented scene in physical pixels, bottom first; empty before the first present.
        pub pieces: RefCell<Vec<Piece>>,
        /// The sideband widgets of the presented scene (children of this view) and where each lands, physical pixels.
        pub placed: RefCell<Vec<(gtk4::Widget, FloatRect)>>,
        /// The natural size in physical pixels (what the window asked for), 0 x 0 until set; `measure` converts it at the current scale. The minimum is always 0, so the user can shrink the window.
        pub wanted: Cell<atl_platform_ui_api::PhysSize>,
        /// The last allocation reported to `on_allocate`, logical pixels: the same size is not reported twice.
        pub allocated: Cell<(i32, i32)>,
        /// Receives the content's size in physical pixels when a layout gives it a new one ([`super::SceneView::connect_allocation`]).
        pub on_allocate: RefCell<Option<AllocateFn>>,
        /// Time the last snapshot took (ns), until the window's paint counter takes it.
        pub snapshot_ns: Cell<Option<i64>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SceneView {
        const NAME: &'static str = "AtlSceneView";
        type Type = super::SceneView;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for SceneView {
        fn constructed(&self) {
            self.parent_constructed();
            let widget = self.obj();
            widget.set_hexpand(true);
            widget.set_vexpand(true);
            // quads outside the content area must not paint over the window chrome
            widget.set_overflow(gtk4::Overflow::Hidden);
        }

        fn dispose(&self) {
            // GTK requires a widget's children unparented before it goes
            self.placed.borrow_mut().clear();
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for SceneView {
        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let wanted = self.wanted.get();
            let physical = match orientation {
                gtk4::Orientation::Horizontal => wanted.width,
                _ => wanted.height,
            };
            // derived at each layout, never stored: the scale is not final until the surface has its fractional scale (GTK's integer scale_factor before it: 2 on a 1.6 output)
            let natural = logical_px(self.obj().scale(), physical);
            (0, natural, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.obj().allocate_children();
            if width <= 0 || height <= 0 || self.allocated.get() == (width, height) {
                return;
            }
            self.allocated.set((width, height));
            let scale = self.obj().scale();
            let physical = atl_platform_ui_api::PhysSize {
                width: scale.to_physical(f64::from(width)).round() as u32,
                height: scale.to_physical(f64::from(height)).round() as u32,
            };
            if let Some(on_allocate) = self.on_allocate.borrow_mut().as_mut() {
                on_allocate(physical);
            }
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let start = crate::window::monotonic_ns();
            self.snapshot_scene(snapshot);
            self.snapshot_ns
                .set(Some(crate::window::monotonic_ns() - start));
        }
    }

    impl SceneView {
        pub(super) fn snapshot_scene(&self, snapshot: &gtk4::Snapshot) {
            let widget = self.obj();
            // an Android display shows black where no layer covers it
            snapshot.append_color(
                &gdk::RGBA::BLACK,
                &graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32),
            );
            let to_logical = (1.0 / widget.scale().get()) as f32;
            let placed = self.placed.borrow();
            for piece in self.pieces.borrow().iter() {
                match piece {
                    Piece::Node(node) => {
                        snapshot.save();
                        snapshot.scale(to_logical, to_logical);
                        snapshot.append_node(node);
                        snapshot.restore();
                    }
                    Piece::Sideband {
                        stream,
                        clip,
                        alpha,
                        ..
                    } => {
                        let Some(child) = resolve(stream).filter(|c| {
                            c.parent().as_ref() == Some(widget.upcast_ref())
                                && placed.iter().any(|(p, _)| p == c)
                        }) else {
                            continue;
                        };
                        snapshot.push_clip(&logical_rect(clip, to_logical));
                        if *alpha < 1.0 {
                            snapshot.push_opacity(f64::from(*alpha));
                        }
                        widget.snapshot_child(&child, snapshot);
                        if *alpha < 1.0 {
                            snapshot.pop();
                        }
                        snapshot.pop();
                    }
                }
            }
        }
    }
}

glib::wrapper! {
    /// The content widget of a window.
    pub struct SceneView(ObjectSubclass<imp::SceneView>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl SceneView {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Replaces the scene shown at the next frame. Each sideband piece naming a registered widget makes it a child of this view (taken from another view first) placed at the piece;
    /// children no piece names stay parented but hidden (`child_visible` false: neither drawn nor picked), so a layer missing for a frame does not unrealize the page.
    pub fn show_pieces(&self, pieces: Vec<Piece>) {
        let mut placed = Vec::new();
        for piece in &pieces {
            let Piece::Sideband { stream, dst, .. } = piece else {
                continue;
            };
            let Some(child) = resolve(stream) else {
                if SIDEBAND_UNRESOLVED.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::warn!(
                        "atl-platform-ui: sideband stream {stream:?} names no widget: drawn as nothing (first of a count)"
                    );
                }
                continue;
            };
            if child.parent().as_ref() != Some(self.upcast_ref()) {
                if child.parent().is_some() {
                    child.unparent();
                }
                child.set_parent(self);
                tracing::info!("atl-platform-ui: sideband widget parented, stream {stream:?}");
            }
            placed.push((child, *dst));
        }
        let mut next = self.first_child();
        while let Some(child) = next {
            let shown = placed.iter().any(|(p, _)| *p == child);
            if child.is_child_visible() != shown {
                // a transition, not a frame: the scene gained or lost the stream's quad (layer hidden, alpha 0, clipped out, removed); the first lines carry the time, then counts only
                let what = if shown {
                    "sideband.shown"
                } else {
                    "sideband.hidden"
                };
                if crate::count::hit(what) <= TRANSITIONS_LOGGED {
                    tracing::info!(
                        "atl-platform-ui: sideband key {:?} {}",
                        crate::sideband::key_of(&child),
                        if shown {
                            "shown"
                        } else {
                            "hidden: no quad names it"
                        }
                    );
                }
            }
            child.set_child_visible(shown);
            next = child.next_sibling();
        }
        let moved = *self.imp().placed.borrow() != placed;
        if moved {
            for piece in &pieces {
                if let Piece::Sideband {
                    stream,
                    dst,
                    clip,
                    alpha,
                } = piece
                {
                    tracing::info!(
                        "atl-platform-ui: sideband {stream:?} placed dst={},{},{},{} clip={},{},{},{} alpha={alpha}",
                        dst.left,
                        dst.top,
                        dst.right,
                        dst.bottom,
                        clip.left,
                        clip.top,
                        clip.right,
                        clip.bottom
                    );
                }
            }
        }
        self.imp().placed.replace(placed);
        self.imp().pieces.replace(pieces);
        if moved {
            self.queue_allocate();
        }
        self.queue_draw();
    }

    /// The time of the snapshot since the last call (ns), None when the view did not snapshot.
    pub fn take_snapshot_ns(&self) -> Option<i64> {
        self.imp().snapshot_ns.take()
    }

    /// Allocates each placed child at its quad, in logical pixels (GTK picking follows the transform).
    fn allocate_children(&self) {
        let to_logical = (1.0 / self.scale().get()) as f32;
        for (child, dst) in self.imp().placed.borrow().iter() {
            let r = logical_rect(dst, to_logical);
            let transform = gsk::Transform::new().translate(&graphene::Point::new(r.x(), r.y()));
            child.allocate(
                r.width().round() as i32,
                r.height().round() as i32,
                -1,
                Some(transform),
            );
        }
    }

    /// What the view draws now (its scene and the sideband children), in physical pixels: the picture `Window::capture` reads.
    /// The view's own snapshot of the presented pieces, not `gtk4::WidgetPaintable`: that shows the widget's last painted render node, so a capture right after `present` read the frame before.
    pub fn picture(&self) -> Option<gsk::RenderNode> {
        let scale = self.scale().get() as f32;
        let snapshot = gtk4::Snapshot::new();
        snapshot.scale(scale, scale);
        self.imp().snapshot_scene(&snapshot);
        snapshot.to_node()
    }

    /// Calls `on_allocate` with the content's size in physical pixels when a layout gives it a new one (not twice for the same size).
    pub fn connect_allocation<F: FnMut(atl_platform_ui_api::PhysSize) + 'static>(
        &self,
        on_allocate: F,
    ) {
        *self.imp().on_allocate.borrow_mut() = Some(Box::new(on_allocate));
    }

    /// Sets the natural size, in physical pixels, and asks for a new layout.
    pub fn set_wanted(&self, size: atl_platform_ui_api::PhysSize) {
        self.imp().wanted.set(size);
        self.queue_resize();
    }

    /// The natural size last set by [`SceneView::set_wanted`], in physical pixels: the scale-free value the window's logical size is derived from.
    pub fn wanted(&self) -> atl_platform_ui_api::PhysSize {
        self.imp().wanted.get()
    }

    /// The surface's scale changed: lay out again, so the natural size follows it and the next allocation is reported in the new physical pixels even at the same logical size.
    pub fn rescale(&self) {
        self.imp().allocated.set((0, 0));
        self.queue_resize();
    }

    /// Physical pixels per logical pixel on the output the window is on ([`output_scale`]).
    pub fn scale(&self) -> atl_platform_ui_api::Scale {
        let surface = self.native().and_then(|n| n.surface());
        let display = self.display();
        let monitor = surface
            .as_ref()
            .and_then(|s| display.monitor_at_surface(s))
            .or_else(|| first_monitor(&display));
        let scale = output_scale(
            surface.as_ref().map(|s| s.scale()),
            monitor.as_ref().map(|m| m.scale()),
            f64::from(self.scale_factor()),
        );
        atl_platform_ui_api::Scale::new(scale).unwrap_or(atl_platform_ui_api::Scale::ONE)
    }
}

#[cfg(test)]
mod tests {
    use super::{logical_px, output_scale};
    use atl_platform_ui_api::Scale;

    /// Derived by hand: 1125 / 1.6 = 703.125 -> 703 (703 x 1.6 = 1124.8 -> 1125); 1350 / 1.6 = 843.75 -> 844 (1350.4 -> 1350). ceil would give 704 -> 1126.
    #[test]
    fn logical_size_maps_back_to_the_nearest_physical() {
        let scale = Scale::new(1.6).unwrap();
        assert_eq!(logical_px(scale, 1125), 703);
        assert_eq!(logical_px(scale, 1350), 844);
        assert_eq!(logical_px(scale, 320), 200);
        assert_eq!(logical_px(scale, 240), 150);
        assert_eq!(logical_px(Scale::ONE, 320), 320);
    }

    /// Derived by hand: the laptop's Task is 1125 x 1350 px (AOSP at 2880 x 1800 px, density 1.6). At 1.6 the content is 703 x 844 (1125 / 1.6 = 703.125, 1350 / 1.6 = 843.75). At GDK's
    /// integer 2 it is 563 x 675 (562.5 -> 563, 675): the size the floating window kept.
    #[test]
    fn first_commit_is_measured_at_the_output_scale() {
        let scale = Scale::new(output_scale(Some(2.0), Some(1.6), 2.0)).unwrap();
        assert_eq!(
            (logical_px(scale, 1125), logical_px(scale, 1350)),
            (703, 844)
        );
        // a fractional surface scale is final even against another monitor's scale
        assert_eq!(output_scale(Some(1.6), Some(2.0), 2.0), 1.6);
        // an integer surface scale on an integer output is that scale
        assert_eq!(output_scale(Some(2.0), Some(2.0), 1.0), 2.0);
        assert_eq!(output_scale(Some(2.0), None, 1.0), 2.0);
        // before the surface is on an output: the first monitor; with neither, the widget's
        assert_eq!(output_scale(None, Some(1.6), 1.0), 1.6);
        assert_eq!(output_scale(None, None, 2.0), 2.0);
    }
}
