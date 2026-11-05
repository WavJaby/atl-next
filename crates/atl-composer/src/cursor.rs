// SPDX-License-Identifier: MIT
//! Android pointer icon types (PointerIcon.TYPE_*, values from libs/input PointerIconType.aidl) as platform cursor shapes: the Android side of the cursor seam.

use atl_platform_ui::CursorShape;

/// The shape for icon type `kind`; None for a type with no desktop cursor: CUSTOM (-1: bitmaps, refused by the parcel codec), the spot icons (2000-2002: touch feedback) and HANDWRITING (1022), and unknown values.
/// NOT_SPECIFIED (1) is what ViewRootImpl.updatePointerIcon sends when no view resolves an icon (a Button, a plain area): the default arrow.
pub fn shape(kind: i32) -> Option<CursorShape> {
    Some(match kind {
        0 => CursorShape::None,
        1 | 1000 => CursorShape::Default,
        1001 => CursorShape::ContextMenu,
        1002 => CursorShape::Pointer,
        1003 => CursorShape::Help,
        1004 => CursorShape::Wait,
        1006 => CursorShape::Cell,
        1007 => CursorShape::Crosshair,
        1008 => CursorShape::Text,
        1009 => CursorShape::VerticalText,
        1010 => CursorShape::Alias,
        1011 => CursorShape::Copy,
        1012 => CursorShape::NoDrop,
        1013 => CursorShape::AllScroll,
        1014 => CursorShape::EwResize,
        1015 => CursorShape::NsResize,
        1016 => CursorShape::NeswResize,
        1017 => CursorShape::NwseResize,
        1018 => CursorShape::ZoomIn,
        1019 => CursorShape::ZoomOut,
        1020 => CursorShape::Grab,
        1021 => CursorShape::Grabbing,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PointerIconType.aidl: ARROW 1000 and NOT_SPECIFIED 1 the arrow, HAND 1002, TEXT 1008, TYPE_NULL 0; TOP_RIGHT_DOUBLE_ARROW 1016 is the "/" diagonal (nesw); SPOT_HOVER 2000 and the unused 1005 have no cursor.
    #[test]
    fn types_map_to_css_shapes() {
        assert_eq!(shape(1000), Some(CursorShape::Default));
        assert_eq!(shape(1), Some(CursorShape::Default));
        assert_eq!(shape(1002), Some(CursorShape::Pointer));
        assert_eq!(shape(1008), Some(CursorShape::Text));
        assert_eq!(shape(0), Some(CursorShape::None));
        assert_eq!(shape(1016), Some(CursorShape::NeswResize));
        assert_eq!((shape(2000), shape(1005)), (None, None));
    }
}
