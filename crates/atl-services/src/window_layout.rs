// SPDX-License-Identifier: MIT
//! Window frames inside their parent frame, as WindowLayout.computeFrames places them: Gravity.apply, then Gravity.applyDisplay
//! (frameworks/base core/java/android/view/Gravity.java, axis by axis; margins not modelled).

use crate::tasks::Placement;

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

/// Gravity.DISPLAY_CLIP_VERTICAL / DISPLAY_CLIP_HORIZONTAL (Gravity.java:111, :118): clip to the display instead of moving into it.
const DISPLAY_CLIP_VERTICAL: i32 = 0x1000_0000;
const DISPLAY_CLIP_HORIZONTAL: i32 = 0x0100_0000;

/// One axis of Gravity.applyDisplay: `[lo, hi)` moved into `[start, end)`, or clipped with `clip`; longer than the display -> the display.
fn fit_axis(clip: bool, (lo, hi): (i32, i32), (start, end): (i32, i32)) -> (i32, i32) {
    if clip {
        return (lo.max(start), hi.min(end));
    }
    let off = if lo < start {
        start - lo
    } else if hi > end {
        end - hi
    } else {
        0
    };
    if off == 0 {
        (lo, hi)
    } else if hi - lo > end - start {
        (start, end)
    } else {
        (lo + off, hi + off)
    }
}

/// Gravity.applyDisplay(gravity, display, frame) (Gravity.java:350-385): the frame moved (or clipped, per DISPLAY_CLIP_*) to lie inside `display`.
pub fn fit_display(
    gravity: i32,
    (dl, dt, dr, db): (i32, i32, i32, i32),
    (l, t, r, b): (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let (t, b) = fit_axis(gravity & DISPLAY_CLIP_VERTICAL != 0, (t, b), (dt, db));
    let (l, r) = fit_axis(gravity & DISPLAY_CLIP_HORIZONTAL != 0, (l, r), (dl, dr));
    (l, t, r, b)
}

/// LayoutParams.FLAG_LAYOUT_NO_LIMITS: the window may extend past the display frame (WindowLayout skips Gravity.applyDisplay).
const FLAG_LAYOUT_NO_LIMITS: i32 = 0x200;
/// LayoutParams.TYPE_BASE_APPLICATION: an activity's own window, never moved into the display frame in multi-window (WindowLayout.java:255-256).
const TYPE_BASE_APPLICATION: i32 = 1;
/// LayoutParams.MATCH_PARENT.
const MATCH_PARENT: i32 = -1;

/// One axis of a window's size (WindowLayout.java:192-198, :216-229): MATCH_PARENT -> the parent's; else the size the client asked for at relayout; before the first relayout
/// (UNSPECIFIED_LENGTH) the LayoutParams size when one is set, else the parent's.
fn window_length(attr: i32, requested: i32, parent: i32) -> i32 {
    if attr == MATCH_PARENT {
        parent
    } else if requested > 0 {
        requested
    } else if attr >= 0 {
        attr
    } else {
        parent
    }
}

/// A freeform window's frame in `parent` (= display frame = the Task's bounds; WindowLayout.java:86-90): its size ([`window_length`], at most the parent's in multi-window, :240-245)
/// placed by gravity and x/y (:259 Gravity.apply), then moved inside the display frame unless it is the activity's own window or FLAG_LAYOUT_NO_LIMITS (:255-266 fitToDisplay,
/// Gravity.applyDisplay): a popup near the bottom stays inside the Task. A MATCH_PARENT window fills the Task.
pub fn frame(p: &Placement, parent: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let size = (
        window_length(p.size.0, p.requested.0, parent.2 - parent.0),
        window_length(p.size.1, p.requested.1, parent.3 - parent.1),
    );
    let placed = gravity_frame(p.gravity, size, (p.x, p.y), parent);
    if p.r#type != TYPE_BASE_APPLICATION && p.flags & FLAG_LAYOUT_NO_LIMITS == 0 {
        fit_display(p.gravity, parent, placed)
    } else {
        placed
    }
}

/// Where a window's layer goes in its Task: the frame's origin less the surface insets (WindowState.transformFrameToSurfacePosition, WindowState.java:5190-5218: the surface
/// extends past the frame by surfaceInsets, e.g. a dialog's shadow, so its top-left sits that far up and left of the frame).
pub fn surface_origin(p: &Placement, (left, top): (i32, i32)) -> (i32, i32) {
    (left - p.surface_inset.0, top - p.surface_inset.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::FLAG_DIM_BEHIND;

    /// LayoutParams.TYPE_APPLICATION (a Dialog's window), FIRST_SUB_WINDOW (a popup), WRAP_CONTENT; Gravity TOP|LEFT.
    const TYPE_APPLICATION: i32 = 2;
    const FIRST_SUB_WINDOW: i32 = 1000;
    const WRAP_CONTENT: i32 = -2;
    const TOP_LEFT: i32 = 0x33;

    /// Expected frames by hand from WindowLayout.computeFrames + Gravity.apply/applyDisplay on a 960x600 Task.
    #[test]
    fn frames_follow_window_layout() {
        let task = (0, 0, 960, 600);
        let activity = Placement {
            r#type: TYPE_BASE_APPLICATION,
            size: (MATCH_PARENT, MATCH_PARENT),
            requested: (960, 600),
            ..Placement::default()
        };
        assert_eq!(frame(&activity, task), task);
        // resized: MATCH_PARENT takes the new bounds, not the stale requested size
        assert_eq!(frame(&activity, (0, 0, 1200, 800)), (0, 0, 1200, 800));
        // a WRAP_CONTENT dialog measured 300x200, gravity 0 = centred: ((960-300)/2, (600-200)/2)
        let dialog = Placement {
            r#type: TYPE_APPLICATION,
            size: (WRAP_CONTENT, WRAP_CONTENT),
            requested: (300, 200),
            ..Placement::default()
        };
        assert_eq!(frame(&dialog, task), (330, 200, 630, 400));
        // before its first relayout a WRAP_CONTENT window has no size: it fills the parent
        let unmeasured = Placement {
            requested: (0, 0),
            ..dialog
        };
        assert_eq!(frame(&unmeasured, task), task);
        // a 100x40 popup asked at y 580 would end at 620: moved up into the Task, 560..600
        let popup = Placement {
            r#type: FIRST_SUB_WINDOW,
            gravity: TOP_LEFT,
            x: 10,
            y: 580,
            size: (100, 40),
            requested: (100, 40),
            ..Placement::default()
        };
        assert_eq!(frame(&popup, task), (10, 560, 110, 600));
        // FLAG_LAYOUT_NO_LIMITS: left where it asked
        let unlimited = Placement {
            flags: FLAG_LAYOUT_NO_LIMITS,
            ..popup
        };
        assert_eq!(frame(&unlimited, task), (10, 580, 110, 620));
        // the activity's own window is never moved into the display frame (fitToDisplay false)
        let base = Placement {
            r#type: TYPE_BASE_APPLICATION,
            ..popup
        };
        assert_eq!(frame(&base, task), (10, 580, 110, 620));
    }

    /// The surface sits surfaceInsets up and left of the frame; the dim is dimAmount only while FLAG_DIM_BEHIND is set.
    #[test]
    fn surface_origin_and_dim() {
        let p = Placement {
            surface_inset: (32, 24),
            dim_amount: 0.5,
            ..Placement::default()
        };
        assert_eq!(surface_origin(&p, (330, 200)), (298, 176));
        assert_eq!(p.dim(), 0.0);
        let dimming = Placement {
            flags: FLAG_DIM_BEHIND,
            ..p
        };
        assert_eq!(dimming.dim(), 0.5);
    }

    /// Gravity.applyDisplay by hand on a 960x600 display: a 100x40 frame 20 px past the bottom moves up by 20; with DISPLAY_CLIP_VERTICAL it is cut at the bottom;
    /// one past the left moves right; a frame inside stays; one taller than the display becomes the display's height.
    #[test]
    fn fits_the_display() {
        let display = (0, 0, 960, 600);
        assert_eq!(
            fit_display(0, display, (10, 580, 110, 620)),
            (10, 560, 110, 600)
        );
        assert_eq!(
            fit_display(DISPLAY_CLIP_VERTICAL, display, (10, 580, 110, 620)),
            (10, 580, 110, 600)
        );
        assert_eq!(fit_display(0, display, (-30, 0, 70, 40)), (0, 0, 100, 40));
        assert_eq!(
            fit_display(0, display, (10, 10, 110, 50)),
            (10, 10, 110, 50)
        );
        assert_eq!(
            fit_display(0, display, (0, -10, 100, 700)),
            (0, 0, 100, 600)
        );
    }

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
