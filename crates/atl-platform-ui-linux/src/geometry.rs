// SPDX-License-Identifier: MIT
//! Where a quad's buffer lands on the screen: the 2D map from buffer pixels to layer-stack pixels, pure arithmetic so it is tested without a display.

use atl_compositor::state::FloatRect;

/// `HAL_TRANSFORM_FLIP_H`, `_FLIP_V`, `_ROT_90` (system/graphics-base-v1.0.h: `1`, `2`, `4`).
pub const FLIP_H: u32 = 1;
pub const FLIP_V: u32 = 2;
pub const ROT_90: u32 = 4;

/// A 2D affine map, `x' = xx*x + xy*y + x0`, `y' = yx*x + yy*y + y0`. Test reference only: the product builds GSK transforms from `Placement`'s steps.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub xx: f64,
    pub yx: f64,
    pub xy: f64,
    pub yy: f64,
    pub x0: f64,
    pub y0: f64,
}

#[cfg(test)]
impl Affine {
    pub const IDENTITY: Affine = Affine {
        xx: 1.0,
        yx: 0.0,
        xy: 0.0,
        yy: 1.0,
        x0: 0.0,
        y0: 0.0,
    };

    pub fn translate(tx: f64, ty: f64) -> Affine {
        Affine {
            x0: tx,
            y0: ty,
            ..Affine::IDENTITY
        }
    }

    pub fn scale(sx: f64, sy: f64) -> Affine {
        Affine {
            xx: sx,
            yy: sy,
            ..Affine::IDENTITY
        }
    }

    /// The map that applies `self` first and `next` second.
    pub fn then(self, next: Affine) -> Affine {
        Affine {
            xx: next.xx * self.xx + next.xy * self.yx,
            yx: next.yx * self.xx + next.yy * self.yx,
            xy: next.xx * self.xy + next.xy * self.yy,
            yy: next.yx * self.xy + next.yy * self.yy,
            x0: next.xx * self.x0 + next.xy * self.y0 + next.x0,
            y0: next.yx * self.x0 + next.yy * self.y0 + next.y0,
        }
    }

    pub fn apply(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.xx * x + self.xy * y + self.x0,
            self.yx * x + self.yy * y + self.y0,
        )
    }
}

/// Size of a rectangle.
fn extent(r: &FloatRect) -> (f64, f64) {
    (f64::from(r.right - r.left), f64::from(r.bottom - r.top))
}

fn center(r: &FloatRect) -> (f64, f64) {
    (
        f64::from(r.left + r.right) / 2.0,
        f64::from(r.top + r.bottom) / 2.0,
    )
}

/// How the part `src` of a buffer lands on the screen rectangle `dst`, as the steps applied to a buffer pixel in this order: move `src`'s center to the origin, flip (`FLIP_H`, `FLIP_V`), rotate 90 degrees clockwise (`ROT_90`), scale to `dst`'s size, move the origin to `dst`'s center.
/// Steps rather than one matrix because GSK's cairo renderer draws an arbitrary-matrix transform as its error node (hot pink): only translate / scale / rotate keep a 2D category it can draw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub src_center: (f64, f64),
    pub flip_h: bool,
    pub flip_v: bool,
    pub rot_90: bool,
    pub scale: (f64, f64),
    pub dst_center: (f64, f64),
}

#[cfg(test)]
impl Placement {
    /// The same map as one matrix (the reference the GSK transform is tested against).
    pub fn affine(&self) -> Affine {
        let mut map = Affine::translate(-self.src_center.0, -self.src_center.1);
        map = map.then(Affine::scale(
            if self.flip_h { -1.0 } else { 1.0 },
            if self.flip_v { -1.0 } else { 1.0 },
        ));
        if self.rot_90 {
            // clockwise on a y-down screen: right (1, 0) goes to down (0, 1), down (0, 1) to left (-1, 0)
            map = map.then(Affine {
                xx: 0.0,
                yx: 1.0,
                xy: -1.0,
                yy: 0.0,
                x0: 0.0,
                y0: 0.0,
            });
        }
        map.then(Affine::scale(self.scale.0, self.scale.1))
            .then(Affine::translate(self.dst_center.0, self.dst_center.1))
    }
}

/// The placement of `src` on `dst` under the buffer's HAL transform bits.
/// That is libui `Transform::set`'s `R * (H * V)` (frameworks/native/libs/ui/Transform.cpp: flips first, then the rotation) and hardware.h's "rotate source image 90 degrees clockwise"; `dst` already has the rotated extent (the compositor swaps width and height for `ROT_90`, atl-compositor model.rs). `None` when `src` or `dst` is empty.
pub fn placement(src: &FloatRect, dst: &FloatRect, transform: u32) -> Option<Placement> {
    let (sw, sh) = extent(src);
    let (dw, dh) = extent(dst);
    if sw <= 0.0 || sh <= 0.0 || dw <= 0.0 || dh <= 0.0 {
        return None;
    }
    let rot_90 = transform & ROT_90 != 0;
    let (turned_w, turned_h) = if rot_90 { (sh, sw) } else { (sw, sh) };
    Some(Placement {
        src_center: center(src),
        flip_h: transform & FLIP_H != 0,
        flip_v: transform & FLIP_V != 0,
        rot_90,
        scale: (dw / turned_w, dh / turned_h),
        dst_center: center(dst),
    })
}

/// `placement` as one matrix, for the tests that check corners.
#[cfg(test)]
pub fn buffer_to_screen(src: &FloatRect, dst: &FloatRect, transform: u32) -> Option<Affine> {
    placement(src, dst, transform).map(|p| p.affine())
}

/// The part of `dst` that shows: `dst` cut by the scissor `clip`, `None` when nothing is left.
pub fn visible(dst: &FloatRect, clip: Option<&FloatRect>) -> Option<FloatRect> {
    let r = match clip {
        None => *dst,
        Some(c) => FloatRect {
            left: dst.left.max(c.left),
            top: dst.top.max(c.top),
            right: dst.right.min(c.right),
            bottom: dst.bottom.min(c.bottom),
        },
    };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: f32, top: f32, right: f32, bottom: f32) -> FloatRect {
        FloatRect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Source corners: top-left (0,0), top-right (4,0), bottom-right (4,2), bottom-left (0,2) of a 4x2 crop.
    const CORNERS: [(f64, f64); 4] = [(0.0, 0.0), (4.0, 0.0), (4.0, 2.0), (0.0, 2.0)];

    fn mapped(m: &Affine) -> [(f64, f64); 4] {
        CORNERS.map(|(x, y)| m.apply(x, y))
    }

    /// Expected corners are placement facts: unflipped, the top-left corner lands on the destination's top-left (10, 20), and so on around the rectangle.
    #[test]
    fn plain_crop_moves_corners_to_the_matching_corners() {
        let m = buffer_to_screen(&rect(0.0, 0.0, 4.0, 2.0), &rect(10.0, 20.0, 14.0, 22.0), 0)
            .expect("non-empty");
        assert_eq!(
            mapped(&m),
            [(10.0, 20.0), (14.0, 20.0), (14.0, 22.0), (10.0, 22.0)]
        );
    }

    /// FLIP_H mirrors left and right: the source's top-left lands top-right.
    #[test]
    fn flip_h_mirrors_horizontally() {
        let m = buffer_to_screen(
            &rect(0.0, 0.0, 4.0, 2.0),
            &rect(10.0, 20.0, 14.0, 22.0),
            FLIP_H,
        )
        .expect("non-empty");
        assert_eq!(
            mapped(&m),
            [(14.0, 20.0), (10.0, 20.0), (10.0, 22.0), (14.0, 22.0)]
        );
    }

    /// FLIP_V mirrors top and bottom: the source's top-left lands bottom-left.
    #[test]
    fn flip_v_mirrors_vertically() {
        let m = buffer_to_screen(
            &rect(0.0, 0.0, 4.0, 2.0),
            &rect(10.0, 20.0, 14.0, 22.0),
            FLIP_V,
        )
        .expect("non-empty");
        assert_eq!(
            mapped(&m),
            [(10.0, 22.0), (14.0, 22.0), (14.0, 20.0), (10.0, 20.0)]
        );
    }

    /// Rotating a picture 90 degrees clockwise carries its top-left corner to the top-right, top-right to bottom-right: a 4x2 crop fills a 2x4 destination.
    #[test]
    fn rot_90_turns_clockwise() {
        let m = buffer_to_screen(
            &rect(0.0, 0.0, 4.0, 2.0),
            &rect(10.0, 20.0, 12.0, 24.0),
            ROT_90,
        )
        .expect("non-empty");
        assert_eq!(
            mapped(&m),
            [(12.0, 20.0), (12.0, 24.0), (10.0, 24.0), (10.0, 20.0)]
        );
    }

    /// FLIP_H is applied before the rotation (R * (H * V)): the flip puts the top-left at the top-right, the clockwise turn then carries that to the bottom-right.
    #[test]
    fn flips_apply_before_the_rotation() {
        let m = buffer_to_screen(
            &rect(0.0, 0.0, 4.0, 2.0),
            &rect(10.0, 20.0, 12.0, 24.0),
            FLIP_H | ROT_90,
        )
        .expect("non-empty");
        assert_eq!(
            mapped(&m),
            [(12.0, 24.0), (12.0, 20.0), (10.0, 20.0), (10.0, 24.0)]
        );
    }

    /// A crop that does not start at the buffer origin and a destination of another size: the crop's corners land on the destination's corners (scale 2 horizontally, 3 vertically).
    #[test]
    fn crop_and_scale_land_on_the_destination() {
        let m = buffer_to_screen(&rect(8.0, 4.0, 12.0, 6.0), &rect(0.0, 0.0, 8.0, 6.0), 0)
            .expect("non-empty");
        assert_eq!(m.apply(8.0, 4.0), (0.0, 0.0));
        assert_eq!(m.apply(12.0, 6.0), (8.0, 6.0));
    }

    #[test]
    fn empty_rectangles_have_no_map() {
        assert!(
            buffer_to_screen(&rect(0.0, 0.0, 0.0, 2.0), &rect(0.0, 0.0, 4.0, 2.0), 0).is_none()
        );
        assert!(
            buffer_to_screen(&rect(0.0, 0.0, 4.0, 2.0), &rect(1.0, 1.0, 1.0, 5.0), 0).is_none()
        );
    }

    #[test]
    fn visible_is_the_overlap_with_the_scissor() {
        let dst = rect(0.0, 0.0, 10.0, 10.0);
        assert_eq!(visible(&dst, None), Some(dst));
        assert_eq!(
            visible(&dst, Some(&rect(5.0, -3.0, 20.0, 4.0))),
            Some(rect(5.0, 0.0, 10.0, 4.0))
        );
        assert_eq!(visible(&dst, Some(&rect(10.0, 0.0, 20.0, 5.0))), None);
    }
}
