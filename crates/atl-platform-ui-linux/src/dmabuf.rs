// SPDX-License-Identifier: MIT
//! A Scene buffer as a `GdkDmabufTexture`: GSK imports the dma-buf itself (GL: an EGLImage, Vulkan: an imported memory), nothing is copied on the CPU (owner decision O3: dmabuf only, no CPU fallback).
//! The layout (fourcc, modifier, plane offsets and strides) is the one gralloc's handle states (`atl_scene_buffer::DmabufView`); a format/modifier pair the display cannot import is refused here, before GDK would fall back to mmap and a CPU download.
//! Fd lifetime: GDK borrows the plane fds until the texture's release func runs. The texture owns dups of them (closed in that func), so a Scene fd the composer closes after release cannot pull the memory away under a texture GSK still holds.

use std::os::fd::{AsRawFd, OwnedFd};

use atl_gralloc::format::{DRM_FORMAT_ABGR8888, DRM_FORMAT_XBGR8888};
use atl_scene_buffer::DmabufView;
use gtk4::gdk;
use gtk4::prelude::*;

/// The fourcc a layer flagged opaque is shown with: the same memory read without alpha (byte 3 undefined), as the opaque memory-format path did.
fn shown_fourcc(fourcc: u32, opaque: bool) -> u32 {
    match fourcc {
        DRM_FORMAT_ABGR8888 if opaque => DRM_FORMAT_XBGR8888,
        other => other,
    }
}

fn fourcc_name(fourcc: u32) -> String {
    String::from_utf8_lossy(&fourcc.to_le_bytes()).into_owned()
}

/// `view`'s dma-buf as a texture of `display`. Android buffers hold premultiplied colour.
pub fn texture(
    display: &gdk::Display,
    view: &DmabufView<'_>,
    opaque: bool,
) -> Result<gdk::Texture, String> {
    let layout = &view.layout;
    let fourcc = shown_fourcc(layout.fourcc, opaque);
    if !display.dmabuf_formats().contains(fourcc, layout.modifier) {
        return Err(format!(
            "the display cannot import fourcc {} modifier {:#x} as a dmabuf",
            fourcc_name(fourcc),
            layout.modifier
        ));
    }
    let owned = view
        .fds
        .iter()
        .map(|fd| fd.try_clone_to_owned())
        .collect::<Result<Vec<OwnedFd>, _>>()
        .map_err(|e| format!("dup plane fd: {e}"))?;
    let n_planes = u32::try_from(layout.planes.len()).map_err(|e| e.to_string())?;
    let mut builder = gdk::DmabufTextureBuilder::new()
        .set_display(display)
        .set_width(layout.width)
        .set_height(layout.height)
        .set_fourcc(fourcc)
        .set_modifier(layout.modifier)
        .set_premultiplied(true)
        .set_n_planes(n_planes);
    for (i, (plane, fd)) in layout.planes.iter().zip(&owned).enumerate() {
        let i = i as u32;
        let offset = u32::try_from(plane.offset).map_err(|e| format!("plane {i} offset: {e}"))?;
        // SAFETY: `fd` is one of `owned`, which the release func below takes and drops only when GDK calls it, i.e. once the texture no longer uses the fds (gdk_dmabuf_texture_builder_build_with_release_func contract), so every fd stays open while GDK may read it.
        builder = unsafe { builder.set_fd(i, fd.as_raw_fd()) }
            .set_offset(i, offset)
            .set_stride(i, plane.stride_bytes);
    }
    // SAFETY: the fds set above stay open until the release func runs (it owns them); the plane geometry is gralloc's validated handle layout (`DmabufView::of`).
    unsafe { builder.build_with_release_func(move || drop(owned)) }
        .map_err(|e| format!("dmabuf texture: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_reads_rgba_as_rgbx() {
        assert_eq!(shown_fourcc(DRM_FORMAT_ABGR8888, true), DRM_FORMAT_XBGR8888);
        assert_eq!(
            shown_fourcc(DRM_FORMAT_ABGR8888, false),
            DRM_FORMAT_ABGR8888
        );
        assert_eq!(
            shown_fourcc(DRM_FORMAT_XBGR8888, false),
            DRM_FORMAT_XBGR8888
        );
        assert_eq!(fourcc_name(DRM_FORMAT_XBGR8888), "XB24");
    }
}
