// SPDX-License-Identifier: MIT
//! The compositor's output for one frame: z-ordered quads (textured or solid colour) with final geometry. One plain struct; the platform layer (M3 slice 6) maps each quad to a window texture.
//! No parcel type crosses this seam: buffers and fences are [`SceneBuffer`] / [`SceneFd`], opaque descriptors the compositor's front-end owns and the platform's `-<os>` crate resolves.

use std::sync::Arc;

use atl_libgui_parcel::state::{Fence, FloatRect, GraphicBuffer};

/// A descriptor named by a scene: open in this process while any [`Scene`] holding it lives (the front-end closes only descriptors the core no longer names). Opaque: only the platform's `-<os>` crate turns it into an OS handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SceneFd(i32);

impl SceneFd {
    /// Names descriptor `fd`; the caller keeps it open while any scene holding the value lives.
    pub fn new(fd: i32) -> Self {
        Self(fd)
    }

    /// The descriptor number, for the `-<os>` crate that resolves it.
    pub fn raw(self) -> i32 {
        self.0
    }

    /// A fence as the scene carries it: `None` for an invalid (already signalled) fence.
    pub fn of_fence(fence: &Fence) -> Option<Self> {
        fence.0.map(|f| Self(f.fd))
    }
}

/// A quad's buffer: its geometry plus the gralloc native handle (fds + ints, `atl-gralloc` `handle` layout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneBuffer {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Row stride in pixels.
    pub stride: i32,
    /// `PixelFormat`.
    pub format: i32,
    /// `GRALLOC_USAGE_*` bits.
    pub usage: u64,
    /// The producer's `GraphicBuffer` id: the key a release names.
    pub id: u64,
    /// The native handle's fds.
    pub fds: Vec<SceneFd>,
    /// The native handle's ints.
    pub ints: Vec<i32>,
}

impl SceneBuffer {
    /// The scene's view of a buffer a transaction carried.
    pub fn of(b: &GraphicBuffer) -> Self {
        Self {
            width: b.width,
            height: b.height,
            stride: b.stride,
            format: b.format,
            usage: b.usage,
            id: b.id,
            fds: b.fds.iter().map(|f| SceneFd(f.fd)).collect(),
            ints: b.ints.clone(),
        }
    }
}

/// A buffer layer's content: the buffer and how it is sampled.
#[derive(Clone, Debug, PartialEq)]
pub struct BufferContent {
    /// The buffer to sample.
    pub buffer: Arc<SceneBuffer>,
    /// BufferQueue frame number of the buffer (BBQ's own counter).
    pub frame_number: u64,
    /// The sync_file signalled when the producer finished writing; `None` when the buffer was ready.
    pub acquire_fence: Option<SceneFd>,
    /// The part of the buffer shown, in buffer pixels (the buffer crop, or the whole buffer).
    pub src: FloatRect,
    /// The layer is flagged opaque: alpha channel to be ignored.
    pub opaque: bool,
    /// `bufferTransform`: HAL transform bits (flip H = 1, flip V = 2, rotate 90 = 4) the platform applies to the sampled `src`.
    pub buffer_transform: u32,
}

/// What a quad shows: a buffer, or a solid colour (SurfaceFlinger's `fillsColor`: a layer with no buffer and colour components >= 0, LayerSnapshot.cpp).
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    /// The layer's latched buffer.
    Buffer(BufferContent),
    /// Straight (non-premultiplied) r, g, b in [0, 1]; the quad's `alpha` is its opacity.
    Color([f32; 3]),
}

/// One layer's content, placed on the screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Quad {
    /// The layer id the content belongs to.
    pub layer_id: i32,
    /// The layer name (diagnostics).
    pub name: String,
    /// What the quad shows.
    pub content: Content,
    /// Where the content lands on the screen, in layer-stack pixels, before `clip`: a buffer's `src` mapped, a colour layer's bounds (its clip).
    pub dst: FloatRect,
    /// The scissor, in layer-stack pixels: every ancestor's and the layer's own crop; `None` when nothing crops.
    pub clip: Option<FloatRect>,
    /// Combined alpha of the layer and its ancestors, in (0, 1].
    pub alpha: f32,
}

impl Quad {
    /// The buffer content, `None` for a colour quad.
    pub fn buffer(&self) -> Option<&BufferContent> {
        match &self.content {
            Content::Buffer(b) => Some(b),
            Content::Color(_) => None,
        }
    }
}

/// What to draw for one frame of one layer stack, bottom to top.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    /// The compositor's frame counter when this scene was built.
    pub frame: u64,
    /// The layer stack shown.
    pub layer_stack: u32,
    /// Quads, bottom first.
    pub quads: Vec<Quad>,
}
