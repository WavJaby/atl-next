// SPDX-License-Identifier: MIT
//! The api traits over the platform module's vtable (hand-written shims, owner O1): each method turns its arguments into `abi` views or callbacks, calls the module, and maps the status back to the trait's error. Each proxy owns one module handle and ends it on drop.
//! Thread rules are the module objects': the session, windows, text inputs and audio objects hold raw handles, so they are neither Send nor Sync, as the GTK and PipeWire types they stand for; the invoker and the frame requester are Send + Sync like theirs.

use std::cell::RefCell;
use std::path::Path;

use atl_platform_ui_api::abi::{
    self, AudioHandle, BytesOut, Callback, FillCallback, InvokerHandle, LooperAbi, MonitorView,
    PaintedView, PcmHandle, QuadView, RequesterHandle, SceneView, Str, Task, TextInputEventView,
    TextInputHandle, UiHandle, UiVtable, WindowAttributesView, WindowHandle,
};
use atl_platform_ui_api::audio::{AudioError, AudioHost, PcmOutput, PcmSpec};
use atl_platform_ui_api::{
    Capture, ContentHints, ContentPurpose, CursorShape, DeviceNumber, FrameClock, FrameRequester,
    FrameTick, InputEvent, InputSource, Looper, Monitor, Painted, PhysRect, PhysSize, PresentError,
    PresentTiming, Scale, Scene, Surrounding, TextInput, TextInputEvent, Ui, UiError, UiInvoker,
    Window, WindowAttributes,
};

use crate::module::module;

/// The toolkit session in the platform module.
pub struct PlatformUi {
    vt: &'static UiVtable,
    h: *mut UiHandle,
}

impl Ui for PlatformUi {
    type Invoker = PlatformInvoker;

    fn attach(
        looper: Box<dyn Looper>,
        buffer_device: Option<DeviceNumber>,
    ) -> Result<Self, UiError> {
        let vt = module().map_err(UiError::Unavailable)?;
        let mut h = std::ptr::null_mut();
        // SAFETY: the vtable's contract: an owned Looper, an out-parameter and an error sink for this call.
        let (status, err) = BytesOut::collect(|err| unsafe {
            (vt.ui_attach)(
                LooperAbi::new(looper),
                buffer_device.map_or(0, |d| d.0),
                buffer_device.is_some(),
                &mut h,
                err,
            )
        });
        if status != abi::OK {
            return Err(abi::ui_error(status, err));
        }
        Ok(Self { vt, h })
    }

    fn invoker(&self) -> PlatformInvoker {
        PlatformInvoker {
            vt: self.vt,
            // SAFETY: a live session handle.
            h: unsafe { (self.vt.ui_invoker)(self.h) },
        }
    }
}

impl Drop for PlatformUi {
    fn drop(&mut self) {
        // SAFETY: this proxy's handle, ended once.
        unsafe { (self.vt.ui_drop)(self.h) }
    }
}

/// Runs work on the UI thread, from any thread.
pub struct PlatformInvoker {
    vt: &'static UiVtable,
    h: *mut InvokerHandle,
}

// SAFETY: the handle boxes the module's invoker, which is Send + Sync (module.rs `crossing_threads`), and the vtable fns it reaches are callable from any thread.
unsafe impl Send for PlatformInvoker {}
// SAFETY: as Send.
unsafe impl Sync for PlatformInvoker {}

impl UiInvoker for PlatformInvoker {
    fn invoke(&self, f: Box<dyn FnOnce() + Send>) {
        // SAFETY: a live invoker handle; the task's ownership passes to the module.
        unsafe { (self.vt.invoker_invoke)(self.h, Task::new(f)) }
    }
}

impl Drop for PlatformInvoker {
    fn drop(&mut self) {
        // SAFETY: this proxy's handle, ended once.
        unsafe { (self.vt.invoker_drop)(self.h) }
    }
}

/// Per-frame scratch of [`PlatformWindow::present`]: the scene's flat view, reused.
#[derive(Default)]
struct SceneScratch {
    quads: Vec<QuadView>,
    fds: Vec<Vec<i32>>,
}

/// A toplevel window in the platform module.
pub struct PlatformWindow {
    vt: &'static UiVtable,
    h: *mut WindowHandle,
    text: PlatformTextInput,
    scratch: RefCell<SceneScratch>,
}

impl PlatformWindow {
    fn set<T: 'static>(
        &self,
        setter: unsafe extern "C" fn(*mut WindowHandle, Callback<T>),
        f: impl FnMut(&T) + 'static,
    ) {
        // SAFETY: a live window handle; the callback's ownership passes to the module.
        unsafe { setter(self.h, Callback::new(f)) }
    }
}

impl Drop for PlatformWindow {
    fn drop(&mut self) {
        // SAFETY: this proxy's handle, ended once (the text input handle dies with it, and `text` is never used after).
        unsafe { (self.vt.window_drop)(self.h) }
    }
}

impl Window for PlatformWindow {
    type Ui = PlatformUi;
    type TextInput = PlatformTextInput;

    fn text_input(&self) -> &PlatformTextInput {
        &self.text
    }

    fn create(ui: &PlatformUi, attributes: &WindowAttributes) -> Result<Self, UiError> {
        let vt = ui.vt;
        let view = WindowAttributesView::of(attributes);
        let mut h = std::ptr::null_mut();
        // SAFETY: a live session; the view borrows `attributes` for this call.
        let (status, err) =
            BytesOut::collect(|err| unsafe { (vt.window_create)(ui.h, &view, &mut h, err) });
        if status != abi::OK {
            return Err(abi::ui_error(status, err));
        }
        // SAFETY: the new window's handle; the text input lives as long as it.
        let text = unsafe { (vt.window_text_input)(h) };
        Ok(Self {
            vt,
            h,
            text: PlatformTextInput { vt, h: text },
            scratch: RefCell::default(),
        })
    }

    fn set_size(&self, size: PhysSize) {
        // SAFETY: a live window.
        unsafe { (self.vt.window_set_size)(self.h, size) }
    }

    fn set_title(&self, title: &str) {
        // SAFETY: a live window; the title borrowed for this call.
        unsafe { (self.vt.window_set_title)(self.h, Str::of(title)) }
    }

    fn set_cursor(&self, shape: CursorShape) {
        // SAFETY: a live window.
        unsafe { (self.vt.window_set_cursor)(self.h, shape) }
    }

    fn minimize(&self) {
        // SAFETY: a live window.
        unsafe { (self.vt.window_minimize)(self.h) }
    }

    fn size(&self) -> PhysSize {
        // SAFETY: a live window.
        unsafe { (self.vt.window_size)(self.h) }
    }

    fn scale(&self) -> Scale {
        // SAFETY: a live window.
        let factor = unsafe { (self.vt.window_scale)(self.h) };
        // the module reports a valid Scale's factor, so `new` refuses nothing here
        Scale::new(factor).unwrap_or(Scale::ONE)
    }

    fn is_mapped(&self) -> bool {
        // SAFETY: a live window.
        unsafe { (self.vt.window_is_mapped)(self.h) }
    }

    fn is_size_fixed(&self) -> bool {
        // SAFETY: a live window.
        unsafe { (self.vt.window_is_size_fixed)(self.h) }
    }

    fn present(&self, scene: &Scene) -> Result<(), PresentError> {
        let mut scratch = self.scratch.borrow_mut();
        let SceneScratch { quads, fds } = &mut *scratch;
        let view = SceneView::of(scene, quads, fds);
        let mut layer_id = 0;
        // SAFETY: a live window; the view and its scratch outlive the call.
        let (status, err) = BytesOut::collect(|err| unsafe {
            (self.vt.window_present)(self.h, &view, &mut layer_id, err)
        });
        if status == abi::OK {
            Ok(())
        } else {
            Err(PresentError::Buffer {
                layer_id,
                reason: String::from_utf8_lossy(&err).into_owned(),
            })
        }
    }

    fn on_painted<F: FnMut(Painted) + 'static>(&self, mut on_painted: F) {
        self.set(self.vt.window_on_painted, move |v: &PaintedView| {
            // SAFETY: the module's view of one paint, valid for this call and taken once here.
            on_painted(unsafe { v.take() })
        })
    }

    fn capture(&self) -> Result<Capture, UiError> {
        let mut size = PhysSize::default();
        let ((status, rgba), err) = BytesOut::collect(|err| {
            // SAFETY: a live window; the out-parameter and both sinks live for the call.
            BytesOut::collect(|pixels| unsafe {
                (self.vt.window_capture)(self.h, &mut size, pixels, err)
            })
        });
        if status != abi::OK {
            return Err(abi::ui_error(status, err));
        }
        Ok(Capture { size, rgba })
    }

    fn on_resize<F: FnMut(PhysSize) + 'static>(&self, mut on_resize: F) {
        self.set(self.vt.window_on_resize, move |s: &PhysSize| on_resize(*s))
    }

    fn on_monitor<F: FnMut(Monitor) + 'static>(&self, mut on_monitor: F) {
        self.set(self.vt.window_on_monitor, move |m: &MonitorView| {
            on_monitor(m.get())
        })
    }
}

/// [`FrameClock::request_frame`] from any thread.
pub struct PlatformFrameRequester {
    vt: &'static UiVtable,
    h: *mut RequesterHandle,
}

// SAFETY: the handle boxes the module's frame requester, which is Send + Sync (module.rs `crossing_threads`); its vtable fns are callable from any thread.
unsafe impl Send for PlatformFrameRequester {}
// SAFETY: as Send.
unsafe impl Sync for PlatformFrameRequester {}

impl FrameRequester for PlatformFrameRequester {
    fn request_frame(&self) {
        // SAFETY: a live requester handle.
        unsafe { (self.vt.requester_request)(self.h) }
    }
}

impl Drop for PlatformFrameRequester {
    fn drop(&mut self) {
        // SAFETY: this proxy's handle, ended once.
        unsafe { (self.vt.requester_drop)(self.h) }
    }
}

impl FrameClock for PlatformWindow {
    type Requester = PlatformFrameRequester;

    fn request_frame(&self) {
        // SAFETY: a live window.
        unsafe { (self.vt.window_request_frame)(self.h) }
    }

    fn frame_requester(&self) -> PlatformFrameRequester {
        PlatformFrameRequester {
            vt: self.vt,
            // SAFETY: a live window.
            h: unsafe { (self.vt.window_frame_requester)(self.h) },
        }
    }

    fn on_tick<F: FnMut(FrameTick) + 'static>(&self, mut on_tick: F) {
        self.set(self.vt.window_on_tick, move |t: &FrameTick| on_tick(*t))
    }

    fn on_presented<F: FnMut(PresentTiming) + 'static>(&self, mut on_presented: F) {
        self.set(self.vt.window_on_presented, move |t: &PresentTiming| {
            on_presented(*t)
        })
    }
}

impl InputSource for PlatformWindow {
    fn on_events<F: FnMut(InputEvent) + 'static>(&self, mut on_event: F) {
        self.set(self.vt.window_on_events, move |e: &InputEvent| on_event(*e))
    }
}

/// A window's text input in the platform module; lives as long as its window.
pub struct PlatformTextInput {
    vt: &'static UiVtable,
    h: *mut TextInputHandle,
}

impl TextInput for PlatformTextInput {
    fn focus_in(&self) {
        // SAFETY: a live text input (its window alive: this proxy is the window's field).
        unsafe { (self.vt.text_focus_in)(self.h) }
    }

    fn focus_out(&self) {
        // SAFETY: as focus_in.
        unsafe { (self.vt.text_focus_out)(self.h) }
    }

    fn reset(&self) {
        // SAFETY: as focus_in.
        unsafe { (self.vt.text_reset)(self.h) }
    }

    fn set_content(&self, purpose: ContentPurpose, hints: ContentHints) {
        // SAFETY: as focus_in.
        unsafe { (self.vt.text_set_content)(self.h, purpose, hints) }
    }

    fn set_surrounding(&self, surrounding: &Surrounding) {
        // SAFETY: as focus_in; the text borrowed for this call.
        unsafe {
            (self.vt.text_set_surrounding)(
                self.h,
                Str::of(&surrounding.text),
                surrounding.cursor_byte,
                surrounding.anchor_byte,
            )
        }
    }

    fn set_cursor_rect(&self, rect: PhysRect) {
        // SAFETY: as focus_in.
        unsafe { (self.vt.text_set_cursor_rect)(self.h, rect) }
    }

    fn on_events<F: FnMut(TextInputEvent) + 'static>(&self, mut on_event: F) {
        let f = Callback::new(move |v: &TextInputEventView| {
            // SAFETY: the module's view of one event, valid for this call.
            on_event(unsafe { v.get() })
        });
        // SAFETY: as focus_in; the callback's ownership passes to the module.
        unsafe { (self.vt.text_on_events)(self.h, f) }
    }
}

/// The audio server connection in the platform module.
pub struct PlatformAudioHost {
    vt: &'static UiVtable,
    h: *mut AudioHandle,
}

impl AudioHost for PlatformAudioHost {
    type Output = PlatformPcmOutput;

    fn connect(remote: &Path) -> Result<Self, AudioError> {
        let vt = module().map_err(AudioError::Unavailable)?;
        let remote = remote
            .to_str()
            .ok_or_else(|| AudioError::Unavailable(format!("{}: not UTF-8", remote.display())))?;
        let mut h = std::ptr::null_mut();
        // SAFETY: the vtable's contract; the path borrowed for this call.
        let (status, err) =
            BytesOut::collect(|err| unsafe { (vt.audio_connect)(Str::of(remote), &mut h, err) });
        if status != abi::OK {
            return Err(abi::audio_error(status, err));
        }
        Ok(Self { vt, h })
    }

    fn open_output(
        &self,
        want: PcmSpec,
        fill: impl FnMut(&mut [f32]) + Send + 'static,
    ) -> Result<PlatformPcmOutput, AudioError> {
        let mut h = std::ptr::null_mut();
        // SAFETY: a live connection; `fill`'s ownership passes to the module.
        let (status, err) = BytesOut::collect(|err| unsafe {
            (self.vt.audio_open_output)(self.h, want, FillCallback::new(fill), &mut h, err)
        });
        if status != abi::OK {
            return Err(abi::audio_error(status, err));
        }
        Ok(PlatformPcmOutput { vt: self.vt, h })
    }
}

impl Drop for PlatformAudioHost {
    fn drop(&mut self) {
        // SAFETY: this proxy's handle, ended once.
        unsafe { (self.vt.audio_drop)(self.h) }
    }
}

/// One PCM output stream in the platform module.
pub struct PlatformPcmOutput {
    vt: &'static UiVtable,
    h: *mut PcmHandle,
}

impl PcmOutput for PlatformPcmOutput {
    fn spec(&self) -> PcmSpec {
        // SAFETY: a live output.
        unsafe { (self.vt.pcm_spec)(self.h) }
    }

    fn quantum(&self) -> u32 {
        // SAFETY: a live output.
        unsafe { (self.vt.pcm_quantum)(self.h) }
    }

    fn set_active(&self, on: bool) -> Result<(), AudioError> {
        // SAFETY: a live output; the sink lives for the call.
        let (status, err) =
            BytesOut::collect(|err| unsafe { (self.vt.pcm_set_active)(self.h, on, err) });
        if status == abi::OK {
            Ok(())
        } else {
            Err(abi::audio_error(status, err))
        }
    }

    fn rt_fallbacks(&self) -> u64 {
        // SAFETY: a live output.
        unsafe { (self.vt.pcm_rt_fallbacks)(self.h) }
    }

    fn underruns(&self) -> u64 {
        // SAFETY: a live output.
        unsafe { (self.vt.pcm_underruns)(self.h) }
    }
}

impl Drop for PlatformPcmOutput {
    fn drop(&mut self) {
        // SAFETY: this proxy's handle, ended once.
        unsafe { (self.vt.pcm_drop)(self.h) }
    }
}
