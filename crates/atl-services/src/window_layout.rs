// SPDX-License-Identifier: MIT
//! Window frames that are not the Task's: a gravity-placed window (a toast) inside its parent frame, as WMS's DisplayPolicy / WindowLayout compute it with Gravity.apply
//! (frameworks/base core/java/android/view/Gravity.java apply, axis by axis; margins not modelled).

/// Gravity.java constants: axis bits and the values per axis.
const AXIS_X_SHIFT: i32 = 0;
const AXIS_Y_SHIFT: i32 = 4;
const AXIS_SPECIFIED: i32 = 0x1;
const AXIS_PULL_BEFORE: i32 = 0x2;
const AXIS_PULL_AFTER: i32 = 0x4;

/// One axis of Gravity.apply: where a `size` long window starts in `[start, end)` with offset `offset`, for the 4 gravity bits of that axis.
fn axis(bits: i32, size: i32, offset: i32, start: i32, end: i32) -> i32 {
    const BEFORE: i32 = AXIS_SPECIFIED | AXIS_PULL_BEFORE;
    const AFTER: i32 = AXIS_SPECIFIED | AXIS_PULL_AFTER;
    match bits & (AXIS_SPECIFIED | AXIS_PULL_BEFORE | AXIS_PULL_AFTER) {
        BEFORE => start + offset,
        AFTER => end - size - offset,
        // 0 and AXIS_SPECIFIED alone (CENTER_*): centred, the offset added (Gravity.apply's case 0 / AXIS_SPECIFIED)
        _ => start + (end - start - size) / 2 + offset,
    }
}

/// Gravity.apply(gravity, w, h, container, x, y) -> (left, top, right, bottom). A non-positive size (MATCH_PARENT's -1, or not measured) fills that axis.
pub fn gravity_frame(
    gravity: i32,
    (w, h): (i32, i32),
    (x, y): (i32, i32),
    (left, top, right, bottom): (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let w = if w > 0 {
        w.min(right - left)
    } else {
        right - left
    };
    let h = if h > 0 {
        h.min(bottom - top)
    } else {
        bottom - top
    };
    let l = axis((gravity >> AXIS_X_SHIFT) & 0xf, w, x, left, right);
    let t = axis((gravity >> AXIS_Y_SHIFT) & 0xf, h, y, top, bottom);
    (l, t, l + w, t + h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gravity.java: LEFT 0x03, RIGHT 0x05, CENTER_HORIZONTAL 0x01, TOP 0x30, BOTTOM 0x50, CENTER_VERTICAL 0x10. Toast's default is BOTTOM | CENTER_HORIZONTAL (0x51).
    #[test]
    fn places_by_axis() {
        let parent = (0, 0, 960, 600);
        // toast default: 200x50 centred horizontally, its bottom 64 px above the parent's: left (960-200)/2, top 600-50-64
        assert_eq!(
            gravity_frame(0x51, (200, 50), (0, 64), parent),
            (380, 486, 580, 536)
        );
        assert_eq!(
            gravity_frame(0x33, (100, 40), (10, 20), parent),
            (10, 20, 110, 60)
        );
        assert_eq!(
            gravity_frame(0x55, (100, 40), (10, 20), parent),
            (850, 540, 950, 580)
        );
        // CENTER (0x11): (960-100)/2, (600-40)/2
        assert_eq!(
            gravity_frame(0x11, (100, 40), (0, 0), parent),
            (430, 280, 530, 320)
        );
        // unmeasured: fills the parent
        assert_eq!(gravity_frame(0x51, (-1, -1), (0, 0), parent), parent);
    }
}
