// SPDX-License-Identifier: MIT
//! Loading the platform module (launcher-ui-module plan S3): `dlopen(abi::MODULE_FILE, RTLD_NOW | RTLD_LOCAL)` through the dynamic loader's search path (the runtime library dir `LD_LIBRARY_PATH` names, as for `libGLES_atl.so`); no environment switch picks it.
//! Once per process, on the first `PlatformUi::attach` or `PlatformAudioHost::connect`; a failure is kept and every later call gets the same text. The handle is never closed: the module's objects live as long as the process.
//! The identity check reads only `UiVtable::abi` before comparing it with this build's `ABI_ID`; then the module's log records are routed into this process's `tracing` subscriber.

use std::ffi::{CStr, CString, c_void};
use std::sync::OnceLock;

use atl_platform_ui_api::abi::{self, Entry, LogSink, UiVtable};

static MODULE: OnceLock<Result<&'static UiVtable, String>> = OnceLock::new();

/// The module's vtable, loaded on first use; the error names the module and the reason.
pub(crate) fn module() -> Result<&'static UiVtable, String> {
    MODULE.get_or_init(load).clone()
}

fn dlerror() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated string valid until the next dl* call on this thread.
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: non-null, NUL-terminated (above).
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

/// The file the loader resolved `addr`'s object to, or the bare module name.
fn object_path(addr: *const c_void) -> String {
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    // SAFETY: `addr` is a symbol address dlsym returned; `info` is a zeroed out-parameter of the right type.
    if unsafe { libc::dladdr(addr, info.as_mut_ptr()) } == 0 {
        return abi::MODULE_FILE.into();
    }
    // SAFETY: dladdr succeeded, so `info` is filled; dli_fname is null or a NUL-terminated path the loader owns.
    let name = unsafe { info.assume_init() }.dli_fname;
    if name.is_null() {
        return abi::MODULE_FILE.into();
    }
    // SAFETY: non-null, NUL-terminated (above).
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

fn load() -> Result<&'static UiVtable, String> {
    let file = CString::new(abi::MODULE_FILE).map_err(|e| e.to_string())?;
    // SAFETY: a NUL-terminated name; loading runs the module's and its closure's constructors, which is the point. RTLD_LOCAL keeps every symbol of the group out of global lookup.
    let handle = unsafe { libc::dlopen(file.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!(
            "platform module {}: {}",
            abi::MODULE_FILE,
            dlerror()
        ));
    }
    let symbol = CString::new(abi::ENTRY_SYMBOL).map_err(|e| e.to_string())?;
    // SAFETY: a live handle and a NUL-terminated name.
    let entry = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
    let path = object_path(entry);
    if entry.is_null() {
        return Err(format!(
            "platform module {path}: {}: {}",
            abi::ENTRY_SYMBOL,
            dlerror()
        ));
    }
    // SAFETY: the module's `atl_platform_ui_entry`, an `abi::Entry` (the one definition both sides compile).
    let entry: Entry = unsafe { std::mem::transmute::<*mut c_void, Entry>(entry) };
    // SAFETY: the entry has no precondition; it returns its static vtable.
    let vtable =
        unsafe { entry().as_ref() }.ok_or_else(|| format!("platform module {path}: no vtable"))?;
    // the one field read before the check: AbiId leads every build's vtable
    if vtable.abi != abi::ABI_ID {
        return Err(format!(
            "platform module {path}: abi {}, launcher {}",
            vtable.abi,
            abi::ABI_ID
        ));
    }
    // SAFETY: the sink is plain data valid for the process; `forward` is callable from any thread.
    unsafe {
        (vtable.set_log)(LogSink {
            data: std::ptr::null_mut(),
            log: forward,
        })
    };
    tracing::info!(
        "atl-platform-ui: platform module {path} abi {}",
        abi::ABI_ID
    );
    Ok(vtable)
}

/// One module record into this process's subscriber, at its level (the message already carries its `atl-platform-ui:` prefix where the implementation logs one).
unsafe extern "C" fn forward(
    _data: *mut c_void,
    level: abi::Level,
    _target: *const u8,
    _target_len: usize,
    message: *const u8,
    message_len: usize,
) {
    // SAFETY: the LogSink contract: a byte range valid for this call.
    let message =
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(message, message_len) });
    match level {
        1 => tracing::error!(target: "atl_platform_ui_linux", "{message}"),
        2 => tracing::warn!(target: "atl_platform_ui_linux", "{message}"),
        3 => tracing::info!(target: "atl_platform_ui_linux", "{message}"),
        4 => tracing::debug!(target: "atl_platform_ui_linux", "{message}"),
        _ => tracing::trace!(target: "atl_platform_ui_linux", "{message}"),
    }
}
