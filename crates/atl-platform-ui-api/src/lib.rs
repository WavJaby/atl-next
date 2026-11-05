// SPDX-License-Identifier: MIT
//! The app-process half of the platform layer as traits (plan S1 `atl-platform-ui`; M3 slice 6).
//! * [`Ui`]: the toolkit session. Thread-affine: the thread that attaches is the UI thread, which is also the Android Looper's thread (plan Q6, owner 2026-11-05). The Looper is the outer loop: its poll runs the toolkit's fds through the [`Looper`] seam, so a UI thread that stops polling answers no compositor ping and the desktop shows its own "not responding" dialog.
//! * [`Window`]: a toplevel the compositor's [`Scene`] is presented into; [`FrameClock`] and [`InputSource`] are the display's vsync and pointer events of that window.
//!
//! Sizes cross this boundary in physical pixels with an explicit [`Scale`]; no logical-pixel type exists here, because the compositor's layer stack and the app's buffers are physical and a toolkit's logical unit is an implementation detail of the `-<os>` crate.
//! The traits are shaped by the first implementation (GTK4, atl-platform-ui-linux) and by what slices 5 and 7 need of it; they are reshaped when a second implementation is built (CLAUDE.md "One implementation per concept"). Window callbacks are generic parameters (static dispatch) and `'static`: the implementation keeps them for the window's life and calls them on the UI thread from inside the Looper's poll. The [`Looper`] seam takes boxed callbacks: one erasure, because the launcher's callback is a C function pointer.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::os::fd::RawFd;

pub use atl_compositor::scene::Scene;

/// A length in physical (device) pixels: a window's content area, a buffer's extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysSize {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

/// A position in physical pixels from the window content's top-left corner; fractional because pointers report sub-pixel positions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysPoint {
    /// Horizontal distance, in physical pixels.
    pub x: f64,
    /// Vertical distance, in physical pixels.
    pub y: f64,
}

/// Physical pixels per toolkit logical pixel (Wayland buffer scale, possibly fractional). Always finite and positive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale(f64);

impl Scale {
    /// One physical pixel per logical pixel.
    pub const ONE: Scale = Scale(1.0);

    /// `None` unless `factor` is finite and greater than zero.
    pub fn new(factor: f64) -> Option<Scale> {
        (factor.is_finite() && factor > 0.0).then_some(Scale(factor))
    }

    /// The factor.
    pub fn get(self) -> f64 {
        self.0
    }

    /// `physical` pixels expressed in logical pixels.
    pub fn to_logical(self, physical: f64) -> f64 {
        physical / self.0
    }

    /// `logical` pixels expressed in physical pixels.
    pub fn to_physical(self, logical: f64) -> f64 {
        logical * self.0
    }
}

/// Why a [`Ui`] or [`Window`] call failed.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    /// The toolkit cannot start: no display to connect to, or the toolkit refused to initialise.
    #[error("display toolkit unavailable: {0}")]
    Unavailable(String),
    /// The call was made from a thread other than the one that attached the [`Ui`].
    #[error("not on the UI thread")]
    WrongThread,
    /// The Looper refused a registration (or the toolkit's timer could not be made).
    #[error("Android Looper: {0}")]
    Looper(String),
    /// [`Window::capture`] could not produce pixels (window not realised yet, renderer failure).
    #[error("capture failed: {0}")]
    Capture(String),
}

/// Why [`Window::present`] refused a [`Scene`]; the window keeps showing its previous scene.
#[derive(Debug, thiserror::Error)]
pub enum PresentError {
    /// A quad's buffer cannot be read as pixels (unsupported format, no fd, geometry that does not fit the fd, mapping failure).
    #[error("layer {layer_id}: buffer unreadable: {reason}")]
    Buffer {
        /// The quad's layer id.
        layer_id: i32,
        /// What is wrong with the buffer.
        reason: String,
    },
    /// A quad's acquire fence has not signalled. Step 1 presents CPU-written buffers and never waits on the UI thread; the caller presents again after the fence signals.
    #[error("layer {layer_id}: acquire fence not signalled")]
    FenceNotSignalled {
        /// The quad's layer id.
        layer_id: i32,
    },
}

/// The toolkit session. Thread-affine: the thread that attaches is the UI thread.
pub trait Ui: Sized {
    /// Starts the toolkit on the calling thread, which becomes the UI thread, and registers the fds its main loop waits on with `looper`. Called once, when the thread's Looper is prepared and before Java adopts it; never per window.
    fn attach(looper: Box<dyn Looper>) -> Result<Self, UiError>;
}

/// The UI thread's Android Looper as the toolkit's main loop registers its fds with it. The launcher implements it over `ALooper_addFd` / `ALooper_removeFd`; Java's `nativePollOnce` polls it, and each readiness reaches the sink on the UI thread from inside that poll.
pub trait Looper {
    /// Sets the callback run for each readiness of a registered fd. Set once, before the first [`add_fd`](Looper::add_fd).
    fn set_sink(&self, sink: Box<dyn FnMut(RawFd, FdEvents)>);

    /// Registers `fd` for the events it asks for, replacing a registration of the same fd.
    fn add_fd(&self, fd: RawFd, events: FdEvents) -> Result<(), UiError>;

    /// Unregisters `fd`. The Looper may still report it once from the batch it is polling: the sink must ignore an fd it no longer waits on.
    fn remove_fd(&self, fd: RawFd) -> Result<(), UiError>;
}

/// Readiness of a registered fd: `input` and `output` are what the registration asks for; `error` and `hangup` are reported on every poll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FdEvents {
    /// Readable.
    pub input: bool,
    /// Writable.
    pub output: bool,
    /// Error condition.
    pub error: bool,
    /// Peer hung up.
    pub hangup: bool,
}

/// What a new window asks for.
#[derive(Clone, Debug)]
pub struct WindowAttributes {
    /// Title in the window chrome.
    pub title: String,
    /// Content size requested; the compositor may grant another.
    pub size: PhysSize,
}

/// A toplevel window whose content is the compositor's [`Scene`]. The window chrome is the toolkit's; `size` always means the content area inside it.
pub trait Window: FrameClock + InputSource + Sized {
    /// The session this window lives in.
    type Ui: Ui;

    /// Creates the window and shows it; it is mapped (on screen) some main-loop passes later, see [`is_mapped`](Window::is_mapped).
    fn create(ui: &Self::Ui, attributes: &WindowAttributes) -> Result<Self, UiError>;

    /// Asks for a content size. A request, not a command: the shell decides.
    fn set_size(&self, size: PhysSize);

    /// Sets the title shown in the window chrome.
    fn set_title(&self, title: &str);

    /// The content size now: what the compositor granted once mapped, else what was asked for.
    fn size(&self) -> PhysSize;

    /// Physical pixels per logical pixel of the output the window is on.
    fn scale(&self) -> Scale;

    /// The window is on screen: the compositor has configured it and a frame can be shown.
    fn is_mapped(&self) -> bool;

    /// Shows `scene` (quad coordinates are physical pixels of the content area), replacing the previous one; takes effect at the next frame. The buffers are read before this returns, so the caller may reuse them immediately.
    fn present(&self, scene: &Scene) -> Result<(), PresentError>;

    /// What the window content shows now, rendered by the toolkit's own renderer from the presented scene (the platform's screenshot: `UiAutomation.takeScreenshot`, tests).
    fn capture(&self) -> Result<Capture, UiError>;

    /// Calls `on_resize` with the content's size in physical pixels once it has one, then on each change. A tiling compositor resizes every window: the content follows the window, and the caller re-lays out and presents at the new size.
    fn on_resize<F: FnMut(PhysSize) + 'static>(&self, on_resize: F);
}

/// Window content pixels, as [`Window::capture`] returns them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    /// Extent in physical pixels.
    pub size: PhysSize,
    /// `size.width * size.height` pixels of 4 bytes each in memory order R, G, B, A (premultiplied alpha), rows top to bottom with no padding.
    pub rgba: Vec<u8>,
}

/// One frame callback of the display's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameTick {
    /// Start of the frame on the monotonic clock (`CLOCK_MONOTONIC`), in nanoseconds: the vsync timestamp the app's `Choreographer` and the compositor's event thread are fed.
    pub time_ns: u64,
}

/// The display's frame clock for one window: one callback per requested frame, paced by the compositor (Wayland frame callbacks), so a hidden window gets none.
pub trait FrameClock {
    /// Asks for one tick; ticks stop again until the next request (`Choreographer.postFrameCallback`'s one-shot shape).
    fn request_frame(&self);

    /// Sets the callback run on each tick, replacing the previous one.
    fn on_tick<F: FnMut(FrameTick) + 'static>(&self, on_tick: F);
}

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    /// Left, or the touch contact: `BUTTON_PRIMARY`.
    Primary,
    /// Middle: `BUTTON_TERTIARY`.
    Middle,
    /// Right: `BUTTON_SECONDARY`.
    Secondary,
    /// Any other button, by the toolkit's number.
    Other(u32),
}

/// What a pointer did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPhase {
    /// A button went down.
    Down(PointerButton),
    /// A button went up.
    Up(PointerButton),
    /// The pointer moved.
    Move,
}

/// One pointer event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerEvent {
    /// What happened.
    pub phase: PointerPhase,
    /// Where, in physical pixels of the window content.
    pub position: PhysPoint,
    /// Event time in milliseconds on the toolkit's clock (the compositor's event timestamp).
    pub time_ms: u32,
}

/// An input event of a window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    /// Mouse or touchpad pointer. Touch, stylus, keys and gamepads are added by the slice that first consumes them.
    Pointer(PointerEvent),
    /// The window gained (`true`) or lost keyboard focus (the desktop's active toplevel). Android's window focus follows it; on loss, held keys are cancelled by their consumer.
    Focus(bool),
}

/// The input of one window.
pub trait InputSource {
    /// Sets the callback run for each event, replacing the previous one.
    fn on_events<F: FnMut(InputEvent) + 'static>(&self, on_event: F);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scale must be a usable divisor: zero, negative and non-finite factors are refused.
    #[test]
    fn scale_refuses_unusable_factors() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(Scale::new(bad), None, "{bad}");
        }
        assert_eq!(Scale::new(1.5).map(Scale::get), Some(1.5));
    }

    /// 1.5x: 300 physical pixels are 200 logical ones, and back (arithmetic, 300 / 1.5 = 200).
    #[test]
    fn scale_converts_both_ways() {
        let s = Scale::new(1.5).expect("valid");
        assert_eq!(s.to_logical(300.0), 200.0);
        assert_eq!(s.to_physical(200.0), 300.0);
        assert_eq!(Scale::ONE.to_logical(7.0), 7.0);
    }
}
