// SPDX-License-Identifier: MIT
//! A compositor `Scene` as one GSK render node tree, in physical pixels of the window content.
//! Per quad, bottom first: scissor (`clip` cut with the destination) -> opacity (layer alpha) -> buffer-to-screen transform (crop, flips, rotation, scale: `geometry`) -> crop to `src` -> the buffer as a `GdkMemoryTexture` (RGBA_8888 premultiplied; `X8` when the layer is flagged opaque).
//! The widget scales the whole tree by 1 / scale into toolkit logical units when it draws; capture renders it as it is.

use atl_compositor::scene::{Quad, Scene};
use atl_compositor::state::{Fence, FloatRect};
use atl_platform_ui_api::PresentError;
use gtk4::prelude::*;
use gtk4::{gdk, glib, graphene, gsk};

use crate::buffer::{self, Pixels};
use crate::geometry::{Placement, placement, visible};

fn graphene_rect(r: &FloatRect) -> graphene::Rect {
    graphene::Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
}

fn texture(pixels: Pixels, opaque: bool) -> Result<gdk::Texture, String> {
    let format = if opaque {
        gdk::MemoryFormat::R8g8b8x8
    } else {
        gdk::MemoryFormat::R8g8b8a8Premultiplied
    };
    let (width, height) = (
        i32::try_from(pixels.width).map_err(|e| e.to_string())?,
        i32::try_from(pixels.height).map_err(|e| e.to_string())?,
    );
    Ok(gdk::MemoryTexture::new(
        width,
        height,
        format,
        &glib::Bytes::from_owned(pixels.bytes),
        pixels.stride_bytes,
    )
    .upcast())
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

/// The node of one quad, `None` when nothing of it shows (empty rectangles, scissored away).
fn quad_node(quad: &Quad) -> Result<Option<gsk::RenderNode>, PresentError> {
    let fail = |reason: String| PresentError::Buffer {
        layer_id: quad.layer_id,
        reason,
    };
    if let Some(Fence(Some(fence))) = &quad.acquire_fence
        && !buffer::fence_signalled(fence).map_err(fail)?
    {
        return Err(PresentError::FenceNotSignalled {
            layer_id: quad.layer_id,
        });
    }
    let Some(place) = placement(&quad.src, &quad.dst, quad.buffer_transform) else {
        return Ok(None);
    };
    let Some(shown) = visible(&quad.dst, quad.clip.as_ref()) else {
        return Ok(None);
    };
    let pixels = buffer::read_pixels(&quad.buffer).map_err(fail)?;
    let (width, height) = (pixels.width as f32, pixels.height as f32);
    let texture = texture(pixels, quad.opaque).map_err(fail)?;
    let whole = graphene::Rect::new(0.0, 0.0, width, height);
    let sampled = gsk::TextureScaleNode::new(&texture, &whole, gsk::ScalingFilter::Linear);
    let cropped = gsk::ClipNode::new(&sampled, &graphene_rect(&quad.src));
    let placed = gsk::TransformNode::new(&cropped, Some(&gsk_transform(&place)));
    let mut node: gsk::RenderNode = placed.upcast();
    if quad.alpha < 1.0 {
        node = gsk::OpacityNode::new(&node, quad.alpha).upcast();
    }
    Ok(Some(
        gsk::ClipNode::new(&node, &graphene_rect(&shown)).upcast(),
    ))
}

/// The render node of `scene` in physical pixels. Fails without side effects on the first unreadable buffer.
pub fn build(scene: &Scene) -> Result<gsk::RenderNode, PresentError> {
    let children = scene
        .quads
        .iter()
        .map(quad_node)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    Ok(gsk::ContainerNode::new(&children).upcast())
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
