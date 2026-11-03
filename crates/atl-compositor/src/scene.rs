// SPDX-License-Identifier: MIT
//! The compositor's output for one frame: z-ordered textured quads with final geometry. One plain struct; the platform layer (M3 slice 6) maps each quad to a window texture.

use std::sync::Arc;

use crate::state::{Fence, FloatRect, GraphicBuffer};

/// One layer's buffer, placed on the screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Quad {
    /// The layer id the buffer belongs to.
    pub layer_id: i32,
    /// The layer name (diagnostics).
    pub name: String,
    /// The buffer to sample.
    pub buffer: Arc<GraphicBuffer>,
    /// BufferQueue frame number of the buffer (BBQ's own counter).
    pub frame_number: u64,
    /// The fence signalled when the producer finished writing; `None` when the buffer was ready.
    pub acquire_fence: Option<Fence>,
    /// The part of the buffer shown, in buffer pixels (the buffer crop, or the whole buffer).
    pub src: FloatRect,
    /// Where `src` lands on the screen, in layer-stack pixels, before `clip`.
    pub dst: FloatRect,
    /// The scissor, in layer-stack pixels: every ancestor's and the layer's own crop; `None` when nothing crops.
    pub clip: Option<FloatRect>,
    /// Combined alpha of the layer and its ancestors, in (0, 1].
    pub alpha: f32,
    /// The layer is flagged opaque: alpha channel to be ignored.
    pub opaque: bool,
    /// `bufferTransform`: HAL transform bits (flip H = 1, flip V = 2, rotate 90 = 4) the platform applies to the sampled `src`.
    pub buffer_transform: u32,
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
