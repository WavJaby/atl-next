// SPDX-License-Identifier: MIT
//! A compositor `Scene` as one GSK render node tree, in physical pixels of the window content.
//! A colour quad is one colour node over its visible rectangle, the layer alpha as the colour's alpha (GSK colours are straight, like the layer's).
//! Per buffer quad, bottom first: scissor (`clip` cut with the destination) -> opacity (layer alpha) -> buffer-to-screen transform (crop, flips, rotation, scale: `geometry`) -> crop to `src` -> the buffer as a `GdkDmabufTexture` (`dmabuf`; read without alpha when the layer is flagged opaque).
//! The widget scales the whole tree by 1 / scale into toolkit logical units when it draws; capture renders it as it is.

use std::os::fd::OwnedFd;

use atl_compositor::scene::{Content, Quad, Scene};
use atl_libgui_parcel::state::FloatRect;
use atl_platform_ui_api::{DeviceNumber, PresentError};
use atl_scene_buffer::DmabufView;
use gtk4::prelude::*;
use gtk4::{gdk, graphene, gsk};

use crate::dmabuf;
use crate::geometry::{Placement, placement, visible};

fn graphene_rect(r: &FloatRect) -> graphene::Rect {
    graphene::Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
}

/// A buffer the built tree samples: what the window releases once a later painted frame no longer shows it.
#[derive(Debug)]
pub struct Shown {
    /// The quad's layer id.
    pub layer_id: i32,
    /// The buffer's `GraphicBuffer::id`.
    pub buffer_key: u64,
    /// A dup of its first plane's dma-buf: the read fence is exported from it.
    pub dmabuf: OwnedFd,
}

/// `place` as a GSK transform. GSK composes the last call first, so the builder order is the reverse of the order the steps apply in; only translate / scale / rotate are used (see `Placement`).
fn gsk_transform(place: &Placement) -> gsk::Transform {
    let point = |(x, y): (f64, f64)| graphene::Point::new(x as f32, y as f32);
    let mut t = gsk::Transform::new()
        .translate(&point(place.dst_center))
        .scale(place.scale.0 as f32, place.scale.1 as f32);
    if place.rot_90 {
        t = t.rotate(CLOCKWISE_QUARTER_TURN_DEGREES);
    }
    t.scale(
        if place.flip_h { -1.0 } else { 1.0 },
        if place.flip_v { -1.0 } else { 1.0 },
    )
    .translate(&point((-place.src_center.0, -place.src_center.1)))
}

/// GSK rotates positive angles clockwise on its y-down plane, like cairo.
const CLOCKWISE_QUARTER_TURN_DEGREES: f32 = 90.0;

/// The node of one quad, `None` when nothing of it shows (empty rectangles, scissored away); a buffer quad's buffer is added to `shown`.
fn quad_node(
    display: &gdk::Display,
    device: Option<DeviceNumber>,
    quad: &Quad,
    shown: &mut Vec<Shown>,
) -> Result<Option<gsk::RenderNode>, PresentError> {
    let b = match &quad.content {
        Content::Buffer(b) => b,
        // the layer alpha is the colour's own: no opacity node
        Content::Color([r, g, b]) => {
            let Some(shown) = visible(&quad.dst, quad.clip.as_ref()) else {
                return Ok(None);
            };
            let rgba = gdk::RGBA::new(*r, *g, *b, quad.alpha);
            return Ok(Some(
                gsk::ColorNode::new(&rgba, &graphene_rect(&shown)).upcast(),
            ));
        }
    };
    let fail = |reason: String| PresentError::Buffer {
        layer_id: quad.layer_id,
        reason,
    };
    let Some(place) = placement(&b.src, &quad.dst, b.buffer_transform) else {
        return Ok(None);
    };
    let Some(visible_rect) = visible(&quad.dst, quad.clip.as_ref()) else {
        return Ok(None);
    };
    let device = device.ok_or_else(|| {
        fail("no gralloc node in this process: no DRM device to import onto".to_owned())
    })?;
    let view = DmabufView::of(&b.buffer, device).map_err(fail)?;
    let texture = dmabuf::texture(display, &view, b.opaque).map_err(fail)?;
    let first = view
        .fds
        .first()
        .ok_or_else(|| fail("no plane".to_owned()))?;
    shown.push(Shown {
        layer_id: quad.layer_id,
        buffer_key: b.buffer.id,
        dmabuf: first
            .try_clone_to_owned()
            .map_err(|e| fail(format!("dup: {e}")))?,
    });
    let (width, height) = (view.layout.width as f32, view.layout.height as f32);
    let whole = graphene::Rect::new(0.0, 0.0, width, height);
    let sampled = gsk::TextureScaleNode::new(&texture, &whole, gsk::ScalingFilter::Linear);
    let cropped = gsk::ClipNode::new(&sampled, &graphene_rect(&b.src));
    let placed = gsk::TransformNode::new(&cropped, Some(&gsk_transform(&place)));
    let mut node: gsk::RenderNode = placed.upcast();
    if quad.alpha < 1.0 {
        node = gsk::OpacityNode::new(&node, quad.alpha).upcast();
    }
    Ok(Some(
        gsk::ClipNode::new(&node, &graphene_rect(&visible_rect)).upcast(),
    ))
}

/// The render node of `scene` in physical pixels of `display`, with the buffers it samples. Fails without side effects on the first buffer that cannot be imported.
/// `device`: the DRM device every buffer must come from (GtkUi's `buffer_device`).
pub fn build(
    display: &gdk::Display,
    scene: &Scene,
    device: Option<DeviceNumber>,
) -> Result<(gsk::RenderNode, Vec<Shown>), PresentError> {
    let mut shown = Vec::new();
    let mut children = Vec::new();
    for quad in &scene.quads {
        children.extend(quad_node(display, device, quad, &mut shown)?);
    }
    Ok((gsk::ContainerNode::new(&children).upcast(), shown))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GSK transform of every flip / rotation combination equals the reference matrix `Placement::affine` (whose corners the geometry tests pin down), so the builder-order reversal and the rotation sign are right.
    #[test]
    fn gsk_transform_equals_the_reference_matrix() {
        let src = FloatRect {
            left: 8.0,
            top: 4.0,
            right: 40.0,
            bottom: 28.0,
        };
        for bits in 0..8u32 {
            let dst = if bits & 4 != 0 {
                FloatRect {
                    left: 100.0,
                    top: 10.0,
                    right: 124.0,
                    bottom: 42.0,
                }
            } else {
                FloatRect {
                    left: 20.0,
                    top: 30.0,
                    right: 60.0,
                    bottom: 48.0,
                }
            };
            let place = placement(&src, &dst, bits).expect("non-empty");
            let a = place.affine();
            let (xx, yx, xy, yy, dx, dy) = gsk_transform(&place).to_2d();
            let got = [xx, yx, xy, yy, dx, dy].map(f64::from);
            let want = [a.xx, a.yx, a.xy, a.yy, a.x0, a.y0];
            for (g, w) in got.iter().zip(want) {
                assert!(
                    (g - w).abs() < 1e-3,
                    "transform bits {bits}: {got:?} vs {want:?}"
                );
            }
        }
    }
}
