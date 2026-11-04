// SPDX-License-Identifier: MIT
//! `SceneView`: the one widget of a window, drawing the presented scene's render node. No per-widget GTK peers (plan S3): Android renders every pixel of the app, GTK draws only this node and its own chrome.

use std::cell::{Cell, RefCell};

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, gsk};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SceneView {
        /// The presented scene in physical pixels; `None` before the first present.
        pub node: RefCell<Option<gsk::RenderNode>>,
        /// The natural size in logical pixels (what the window asked for), 0 x 0 until set. The minimum is always 0, so the user can shrink the window.
        pub wanted: Cell<(i32, i32)>,
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
    }

    impl WidgetImpl for SceneView {
        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (width, height) = self.wanted.get();
            let natural = match orientation {
                gtk4::Orientation::Horizontal => width,
                _ => height,
            };
            (0, natural, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let widget = self.obj();
            // an Android display shows black where no layer covers it
            snapshot.append_color(
                &gdk::RGBA::BLACK,
                &graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32),
            );
            if let Some(node) = self.node.borrow().as_ref() {
                let to_logical = (1.0 / widget.scale().get()) as f32;
                snapshot.save();
                snapshot.scale(to_logical, to_logical);
                snapshot.append_node(node);
                snapshot.restore();
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

    /// Replaces the scene shown at the next frame.
    pub fn show_node(&self, node: gsk::RenderNode) {
        self.imp().node.replace(Some(node));
        self.queue_draw();
    }

    /// The node last shown, in physical pixels.
    pub fn node(&self) -> Option<gsk::RenderNode> {
        self.imp().node.borrow().clone()
    }

    /// Sets the natural size, in logical pixels, and asks for a new layout.
    pub fn set_wanted(&self, width: i32, height: i32) {
        self.imp().wanted.set((width, height));
        self.queue_resize();
    }

    /// Physical pixels per logical pixel on the output the window is on: the surface's (fractional) scale once it exists, else the integer scale of the widget (1 before realisation).
    pub fn scale(&self) -> atl_platform_ui_api::Scale {
        self.native()
            .and_then(|n| n.surface())
            .map(|s| s.scale())
            .and_then(atl_platform_ui_api::Scale::new)
            .or_else(|| atl_platform_ui_api::Scale::new(f64::from(self.scale_factor())))
            .unwrap_or(atl_platform_ui_api::Scale::ONE)
    }
}
