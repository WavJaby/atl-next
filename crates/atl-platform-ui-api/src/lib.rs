// SPDX-License-Identifier: MIT
//! The app-process half of the platform layer as traits (plan S1 `atl-platform-ui`; M3 slice 6).
//! * [`Ui`]: the toolkit session. Thread-affine: the thread that attaches is the UI thread, which is also the Android Looper's thread (plan Q6, owner 2026-11-05). The Looper is the outer loop: its poll runs the toolkit's fds through the [`Looper`] seam, so a UI thread that stops polling answers no compositor ping and the desktop shows its own "not responding" dialog.
//! * [`Window`]: a toplevel the compositor's [`Scene`] is presented into; [`FrameClock`] and [`InputSource`] are the display's vsync and pointer events of that window.
//!
//! Sizes cross this boundary in physical pixels with an explicit [`Scale`]; no logical-pixel type exists here, because the compositor's layer stack and the app's buffers are physical and a toolkit's logical unit is an implementation detail of the `-<os>` crate.
//! The traits are shaped by the first implementation (GTK4, atl-platform-ui-linux) and by what slices 5 and 7 need of it; they are reshaped when a second implementation is built (CLAUDE.md "One implementation per concept"). Window callbacks are generic parameters (static dispatch) and `'static`: the implementation keeps them for the window's life and calls them on the UI thread from inside the Looper's poll. The [`Looper`] seam takes boxed callbacks: one erasure, because the launcher's callback is a C function pointer.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::os::fd::{OwnedFd, RawFd};

pub use atl_compositor::scene::Scene;
/// The value types the host side shares (a window's monitor is the same type atld reads).
pub use atl_platform_types::{Monitor, PhysSize, Rotation, Scale};

/// A position in physical pixels from the window content's top-left corner; fractional because pointers report sub-pixel positions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysPoint {
    /// Horizontal distance, in physical pixels.
    pub x: f64,
    /// Vertical distance, in physical pixels.
    pub y: f64,
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
    /// A quad's buffer cannot be imported (not a gralloc handle, a format/modifier the display cannot import, the toolkit refused the dma-buf).
    #[error("layer {layer_id}: buffer not importable: {reason}")]
    Buffer {
        /// The quad's layer id.
        layer_id: i32,
        /// What is wrong with the buffer.
        reason: String,
    },
}

/// One frame the toolkit painted after a [`Window::present`]: the buffers that frame no longer shows, which the toolkit read up to that frame.
#[derive(Debug, Default)]
pub struct Painted {
    /// One per buffer shown before and not in the painted scene.
    pub releases: Vec<QuadRelease>,
}

/// A buffer the toolkit stopped showing: the producer may reuse it once `release_fence` signals (`None` = now).
#[derive(Debug)]
pub struct QuadRelease {
    /// The quad's layer id.
    pub layer_id: i32,
    /// The buffer's `GraphicBuffer::id`.
    pub buffer_key: u64,
    /// Signals when the toolkit's GPU reads of the buffer are done.
    pub release_fence: Option<OwnedFd>,
}

/// The toolkit session. Thread-affine: the thread that attaches is the UI thread.
pub trait Ui: Sized {
    /// The handle that runs work on the UI thread from any thread.
    type Invoker: UiInvoker;

    /// Starts the toolkit on the calling thread, which becomes the UI thread, and registers the fds its main loop waits on with `looper`. Called once, when the thread's Looper is prepared and before Java adopts it; never per window.
    fn attach(looper: Box<dyn Looper>) -> Result<Self, UiError>;

    /// A handle that runs closures on this UI thread, from any thread.
    fn invoker(&self) -> Self::Invoker;
}

/// Runs work on the UI thread from another thread (a binder thread's call that must touch a window).
pub trait UiInvoker: Send + Sync + 'static {
    /// Queues `f`; it runs on the UI thread at its next main-loop pass, in queue order.
    fn invoke(&self, f: Box<dyn FnOnce() + Send>);
}

/// A pointer cursor, by the CSS cursor names desktop toolkits share (the set Android's pointer icon types name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    /// The arrow.
    Default,
    /// No cursor shown.
    None,
    /// A context menu is available.
    ContextMenu,
    /// A link or a clickable control (a hand).
    Pointer,
    /// Help is available.
    Help,
    /// Busy.
    Wait,
    /// A table cell.
    Cell,
    /// Precise selection.
    Crosshair,
    /// Text that can be selected or edited.
    Text,
    /// Vertical text.
    VerticalText,
    /// A drag would make an alias.
    Alias,
    /// A drag would copy.
    Copy,
    /// The drop target refuses.
    NoDrop,
    /// Scrollable in any direction.
    AllScroll,
    /// Resize east-west.
    EwResize,
    /// Resize north-south.
    NsResize,
    /// Resize along the "/" diagonal.
    NeswResize,
    /// Resize along the "\" diagonal.
    NwseResize,
    /// Zoom in.
    ZoomIn,
    /// Zoom out.
    ZoomOut,
    /// Can be grabbed.
    Grab,
    /// Being grabbed.
    Grabbing,
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
    /// The window's connection to the desktop's input method.
    type TextInput: TextInput;

    /// The window's one text input: idle until [`TextInput::focus_in`].
    fn text_input(&self) -> &Self::TextInput;

    /// Creates the window and shows it; it is mapped (on screen) some main-loop passes later, see [`is_mapped`](Window::is_mapped).
    fn create(ui: &Self::Ui, attributes: &WindowAttributes) -> Result<Self, UiError>;

    /// Asks for a content size. A request, not a command: the shell decides.
    fn set_size(&self, size: PhysSize);

    /// Sets the title shown in the window chrome.
    fn set_title(&self, title: &str);

    /// Sets the cursor shown while the pointer is over the content.
    fn set_cursor(&self, shape: CursorShape);

    /// The content size now: what the compositor granted once mapped, else what was asked for.
    fn size(&self) -> PhysSize;

    /// Physical pixels per logical pixel of the output the window is on.
    fn scale(&self) -> Scale;

    /// The window is on screen: the compositor has configured it and a frame can be shown.
    fn is_mapped(&self) -> bool;

    /// The shell fixes the content size now (xdg_toplevel state maximized, fullscreen or tiled: the configure's size is binding, xdg-shell.xml): [`set_size`](Window::set_size) is not honoured until that state
    /// ends, and the content keeps the configured size. Otherwise (a floating window) the request is the window's own size to set.
    fn is_size_fixed(&self) -> bool;

    /// Shows `scene` (quad coordinates are physical pixels of the content area; every acquire fence has signalled: the compositor latches only those), replacing the previous one; takes effect at the next frame.
    /// The toolkit reads the buffers on its GPU while it paints, and keeps every buffer of the shown scene until a later scene replaces it: [`on_painted`](Window::on_painted) reports when it stops.
    fn present(&self, scene: &Scene) -> Result<(), PresentError>;

    /// Calls `on_painted` once after each frame the toolkit painted following a present (the frame clock's after-paint), replacing the previous callback. Nothing painted, no call: a hidden window keeps its buffers.
    fn on_painted<F: FnMut(Painted) + 'static>(&self, on_painted: F);

    /// What the window content shows now, rendered by the toolkit's own renderer from the presented scene (the platform's screenshot: `UiAutomation.takeScreenshot`, tests).
    fn capture(&self) -> Result<Capture, UiError>;

    /// Calls `on_resize` with the content's size in physical pixels once it has one, then on each change. A tiling compositor resizes every window: the content follows the window, and the caller re-lays out and presents at the new size.
    fn on_resize<F: FnMut(PhysSize) + 'static>(&self, on_resize: F);

    /// Calls `on_monitor` with the monitor the window is on once it is on one, then each time it moves to another or that monitor's mode, rate or scale changes. On two monitors at once, the one entered last counts.
    fn on_monitor<F: FnMut(Monitor) + 'static>(&self, on_monitor: F);
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

/// [`FrameClock::request_frame`] from another thread.
pub trait FrameRequester: Send + Sync + 'static {
    /// Asks the window's clock for one tick.
    fn request_frame(&self);
}

/// The display's frame clock for one window: one callback per requested frame, paced by the compositor (Wayland frame callbacks), so a hidden window gets none.
pub trait FrameClock {
    /// What [`frame_requester`](FrameClock::frame_requester) hands out.
    type Requester: FrameRequester;

    /// Asks for one tick; ticks stop again until the next request (`Choreographer.postFrameCallback`'s one-shot shape). Requests before the tick coalesce into it.
    fn request_frame(&self);

    /// A handle that requests a tick from any thread (a binder thread's `requestNextVsync` or transaction); the tick still runs on this window's thread. It names the window weakly: once the window is gone, a request does nothing.
    fn frame_requester(&self) -> Self::Requester;

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
    /// The pointer came over the window content.
    Enter,
    /// The pointer left the window content; the position is the last one it had there.
    Leave,
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

/// What a scroll did. Deltas follow the toolkit's convention: positive scrolls toward the end of the content (down, right); the compositor has already applied the desktop's natural-scrolling setting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollKind {
    /// A wheel turned, in notches (fractional for high-resolution wheels).
    Wheel {
        /// Horizontal notches.
        dx: f64,
        /// Vertical notches.
        dy: f64,
    },
    /// Fingers moved on a touchpad (a continuous scroll), in physical pixels.
    Swipe {
        /// Horizontal distance.
        dx: f64,
        /// Vertical distance.
        dy: f64,
    },
    /// The fingers of a [`ScrollKind::Swipe`] sequence lifted.
    SwipeEnd,
}

/// One scroll event, at the pointer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollEvent {
    /// What happened.
    pub kind: ScrollKind,
    /// The pointer position, in physical pixels of the window content.
    pub position: PhysPoint,
    /// Event time in milliseconds on the toolkit's clock.
    pub time_ms: u32,
}

/// What a touch contact did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    /// The contact touched down.
    Down,
    /// The contact moved.
    Move,
    /// The contact lifted.
    Up,
    /// The toolkit or compositor took the contact away (a desktop gesture claimed it): the touch sequence is void.
    Cancel,
}

/// One touch contact event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchEvent {
    /// What happened.
    pub phase: TouchPhase,
    /// The contact: the lowest id no other down contact holds, from its Down to its Up or Cancel (the numbering Android's pointer ids use, TouchInputMapper).
    pub id: u32,
    /// Where, in physical pixels of the window content.
    pub position: PhysPoint,
    /// Event time in milliseconds on the toolkit's clock.
    pub time_ms: u32,
}

/// One physical key press or release of the window's keyboard. No keysym or text: characters come from the Android key map, composed text from `TextInput`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyEvent {
    /// Pressed (`true`) or released. A press of a key already down is the desktop's key repeat.
    pub down: bool,
    /// Linux input event code (`input-event-codes.h`, KEY_A = 30): the identity AOSP `.kl` key layouts key on. A non-Linux backend maps its scancodes to it.
    pub scan_code: u32,
    /// The keyboard's lock states at this event.
    pub locks: Locks,
    /// Event time in milliseconds on the toolkit's clock.
    pub time_ms: u32,
}

/// Lock states of a keyboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Locks {
    /// Caps Lock on.
    pub caps: bool,
    /// Num Lock on.
    pub num: bool,
    /// Scroll Lock on.
    pub scroll: bool,
}

/// An input event of a window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    /// Mouse or touchpad pointer. Stylus and gamepads are added by the slice that first consumes them.
    Pointer(PointerEvent),
    /// A key of the window's keyboard, while the window has keyboard focus.
    Key(KeyEvent),
    /// A touchscreen contact.
    Touch(TouchEvent),
    /// The window gained (`true`) or lost keyboard focus (the desktop's active toplevel). Android's window focus follows it; on loss, held keys are cancelled by their consumer.
    Focus(bool),
    /// Wheel or touchpad scrolling.
    Scroll(ScrollEvent),
}

/// The input of one window.
pub trait InputSource {
    /// Sets the callback run for each event, replacing the previous one.
    fn on_events<F: FnMut(InputEvent) + 'static>(&self, on_event: F);
}

/// A rectangle in physical pixels of the window content, origin top-left.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PhysRect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Extent to the right.
    pub width: f64,
    /// Extent down.
    pub height: f64,
}

/// What a text field holds, for the input method's choice of layout and behaviour (GTK `GtkInputPurpose`, text-input v3 `content_purpose`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContentPurpose {
    /// Any text.
    #[default]
    FreeForm,
    /// Letters only.
    Alpha,
    /// Digits only.
    Digits,
    /// A number, with sign and separators.
    Number,
    /// A phone number.
    Phone,
    /// A URL.
    Url,
    /// An e-mail address.
    Email,
    /// A person's name.
    Name,
    /// A hidden password.
    Password,
    /// A hidden numeric PIN.
    Pin,
    /// A terminal's input.
    Terminal,
}

/// How the input method should treat the text, as independent bits (GTK `GtkInputHints`); combine with `|`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentHints(u32);

impl ContentHints {
    /// No hint.
    pub const NONE: Self = Self(0);
    /// Check spelling.
    pub const SPELLCHECK: Self = Self(1 << 0);
    /// Do not check spelling.
    pub const NO_SPELLCHECK: Self = Self(1 << 1);
    /// Offer word completion.
    pub const WORD_COMPLETION: Self = Self(1 << 2);
    /// Prefer lowercase.
    pub const LOWERCASE: Self = Self(1 << 3);
    /// Every character uppercase.
    pub const UPPERCASE_CHARS: Self = Self(1 << 4);
    /// Each word capitalised.
    pub const UPPERCASE_WORDS: Self = Self(1 << 5);
    /// Each sentence capitalised.
    pub const UPPERCASE_SENTENCES: Self = Self(1 << 6);
    /// Do not show an on-screen keyboard.
    pub const INHIBIT_OSK: Self = Self(1 << 7);
    /// Vertical text.
    pub const VERTICAL_WRITING: Self = Self(1 << 8);
    /// Offer emoji.
    pub const EMOJI: Self = Self(1 << 9);
    /// Do not offer emoji.
    pub const NO_EMOJI: Self = Self(1 << 10);
    /// Sensitive content: the input method should not learn or store it.
    pub const PRIVATE: Self = Self(1 << 11);

    /// Every bit of `other` is set here.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for ContentHints {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// The text around the cursor, as the input method reads it for context. Byte indices into `text` (UTF-8), each on a char boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Surrounding {
    /// The text, usually the paragraph around the cursor.
    pub text: String,
    /// Cursor position.
    pub cursor_byte: usize,
    /// The selection's other end; equal to `cursor_byte` without a selection.
    pub anchor_byte: usize,
}

/// What the input method asks of the focused field. Units are the toolkit's (GTK `GtkIMContext` signals); converting them to Android's is the consumer's (atl-ime `units`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextInputEvent {
    /// The composing text changed: show `text` (UTF-8) as uncommitted, cursor `cursor_chars` chars into it. An empty `text` clears it.
    Preedit {
        /// The composing text.
        text: String,
        /// Cursor offset in chars (Unicode scalar values) from the start of `text`.
        cursor_chars: usize,
    },
    /// Composing ended; the preedit is gone (a [`Commit`](TextInputEvent::Commit) may carry its result).
    PreeditEnd,
    /// Insert this text at the cursor, replacing the preedit.
    Commit(String),
    /// Delete `n_chars` chars starting `offset_chars` chars from the cursor (negative = before it).
    DeleteSurrounding {
        /// Start, in chars relative to the cursor.
        offset_chars: i32,
        /// Length in chars.
        n_chars: u32,
    },
}

/// One window's connection to the desktop's input method (fcitx5, IBus, an on-screen keyboard): the focused Android text field's half of it. All calls on the UI thread.
/// The input method asks for the surrounding text whenever it wants; the implementation answers with the last [`set_surrounding`](TextInput::set_surrounding), so the caller only reports changes.
pub trait TextInput {
    /// A text field gained focus: the input method may start composing for it.
    fn focus_in(&self);
    /// The text field lost focus: composing stops and the input method is released.
    fn focus_out(&self);
    /// The field's text changed outside the input method: the composing state is dropped.
    fn reset(&self);
    /// What the field holds.
    fn set_content(&self, purpose: ContentPurpose, hints: ContentHints);
    /// The text around the cursor, now.
    fn set_surrounding(&self, surrounding: &Surrounding);
    /// Where the cursor is in the window content, for the input method's candidate popup.
    fn set_cursor_rect(&self, rect: PhysRect);
    /// Sets the callback run for each input-method event, replacing the previous one.
    fn on_events<F: FnMut(TextInputEvent) + 'static>(&self, on_event: F);
}
