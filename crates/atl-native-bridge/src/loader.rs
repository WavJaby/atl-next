// SPDX-License-Identifier: MIT
//! The process's guest and the libraries ART asked it to load (NativeBridgeCallbacks `loadLibrary` / `unloadLibrary` / trampolines).
//! The guest starts on the first `loadLibrary`, not at `initialize`: a process without bridged libraries maps nothing (bridge1-step4-plan section 1).
//! Libraries go into the guest linker's default namespace, whose search path is the guest's own libraries plus the directory of every library loaded so far (`__loader_android_update_LD_LIBRARY_PATH`): an APK's `lib/<abi>/` holds the library's `DT_NEEDED` siblings (`libc++_shared.so`) and what it `dlopen`s by name.
//! Any thread: guest calls attach the calling thread first (engine thread.rs). The lock serialises the guest linker's entry points, as bionic's own `g_dl_mutex` would.

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::engine::Down;
use crate::engine::{Config, Guest, active};

/// bionic `RTLD_NOW` (dlfcn.h): what the guest dlopen gets whatever ART's flag; `RTLD_LOCAL` = 0.
const RTLD_NOW: usize = 2;

struct Process {
    guest: Guest,
    dlopen: usize,
    dlsym: usize,
    dlclose: usize,
    dlerror: usize,
    dladdr: usize,
    update_path: usize,
    base_path: Vec<PathBuf>,
    lib_dirs: Vec<PathBuf>,
}

/// Bytes the guest linker reads or writes during one call (a path, a symbol name, a `Dl_info`): a copy in memory the guest can address ([`crate::engine::Active::reserve`]), given back when dropped.
/// A host heap pointer is out of a 32-bit guest's reach (its word truncates it: the arm linker's `split_path` faulted on one), so every loader argument crosses this way, whatever the guest width: one path.
struct GuestBytes {
    at: usize,
    len: usize,
}

impl GuestBytes {
    /// A zeroed copy of `bytes` plus `extra` zero bytes (a C string's NUL, an out buffer).
    fn new(bytes: &[u8], extra: usize) -> Result<GuestBytes, String> {
        let len = bytes.len() + extra;
        let at = active().reserve(len.max(1), (libc::PROT_READ | libc::PROT_WRITE) as u32)?;
        // SAFETY: `reserve` answered `len` fresh writable bytes at `at`, which nothing else refers to.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
        Ok(GuestBytes {
            at,
            len: len.max(1),
        })
    }

    /// The NUL-terminated copy of `s`.
    fn c_str(s: &[u8]) -> Result<GuestBytes, String> {
        if s.contains(&0) {
            return Err(format!("{}: interior NUL", String::from_utf8_lossy(s)));
        }
        GuestBytes::new(s, 1)
    }
}

impl Drop for GuestBytes {
    fn drop(&mut self) {
        active().release(self.at, self.len);
    }
}

/// None = not started; Some(Err) = the start failed (kept: a second start would map the guest twice).
static PROCESS: Mutex<Option<Result<Process, String>>> = Mutex::new(None);

fn linker_fn(g: &Guest, name: &str) -> Result<usize, String> {
    g.linker
        .symbol(name)
        .ok_or_else(|| format!("guest linker lacks {name}"))
}

fn start(cfg: &Config) -> Result<Process, String> {
    let guest = Guest::start(cfg)?;
    Ok(Process {
        dlopen: linker_fn(&guest, "__dl___loader_dlopen")?,
        dlclose: linker_fn(&guest, "__dl___loader_dlclose")?,
        dlerror: linker_fn(&guest, "__dl___loader_dlerror")?,
        dladdr: linker_fn(&guest, "__dl___loader_dladdr")?,
        update_path: linker_fn(&guest, "__dl___loader_android_update_LD_LIBRARY_PATH")?,
        dlsym: linker_fn(&guest, "__dl___loader_dlsym")?,
        guest,
        base_path: cfg.library_path.clone(),
        lib_dirs: Vec::new(),
    })
}

/// Runs `f` on the guest. `cfg` starts it when absent; without one, no guest is an error.
fn with<T>(
    cfg: Option<&dyn Fn() -> Config>,
    f: impl FnOnce(&mut Process) -> Result<T, String>,
) -> Result<T, String> {
    let mut slot = PROCESS
        .lock()
        .map_err(|_| "native bridge state poisoned".to_owned())?;
    if slot.is_none() {
        let cfg = cfg.ok_or("no guest started (nothing loaded)")?;
        *slot = Some(start(&cfg()));
    }
    let Some(p) = slot.as_mut() else {
        return Err("no guest".into());
    };
    let p = p.as_mut().map_err(|e| format!("guest start failed: {e}"))?;
    f(p)
}

impl Process {
    /// The guest linker's last error text.
    fn dlerror(&mut self) -> String {
        // SAFETY: char* __loader_dlerror(void): NULL or a NUL-terminated text in the guest's TLS buffer, read before the next guest call.
        let p = unsafe { self.guest.call(self.dlerror, 0, 0, 0) };
        if p == 0 {
            return "(no guest error)".into();
        }
        // SAFETY: as above.
        unsafe { CStr::from_ptr(p as *const std::ffi::c_char) }
            .to_string_lossy()
            .into_owned()
    }

    fn add_dir(&mut self, dir: &Path) -> Result<(), String> {
        if self.lib_dirs.iter().any(|d| d == dir) {
            return Ok(());
        }
        self.lib_dirs.push(dir.to_owned());
        let joined = std::env::join_paths(self.base_path.iter().chain(&self.lib_dirs))
            .map_err(|e| e.to_string())?;
        let c = GuestBytes::c_str(&joined.into_encoded_bytes())?;
        // SAFETY: void __loader_android_update_LD_LIBRARY_PATH(const char*): the linker copies the text.
        unsafe { self.guest.call(self.update_path, c.at, 0, 0) };
        Ok(())
    }
}

/// Loads `path` into the guest (the config is read only on the first call); the guest handle.
pub fn load(path: &Path, cfg: &dyn Fn() -> Config) -> Result<usize, String> {
    with(Some(cfg), |p| {
        if let Some(dir) = path.parent() {
            p.add_dir(dir)?;
        }
        let c = GuestBytes::c_str(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: void* __loader_dlopen(const char*, int, const void* caller); the caller address puts the library in the main executable's (default) namespace.
        let h = unsafe { p.guest.call(p.dlopen, c.at, RTLD_NOW, p.guest.exe_entry) };
        if h == 0 {
            return Err(format!("guest dlopen {}: {}", path.display(), p.dlerror()));
        }
        // the process's stderr is its log (atl-broker): which library went to the guest, for the gates
        #[allow(clippy::print_stderr, reason = "one line per bridged library")]
        {
            eprintln!(
                "atl-native-bridge: loaded {} into the guest",
                path.display()
            );
        }
        Ok(h)
    })
}

/// Guest `dlsym(handle, name)`; None when absent (or no guest).
pub fn symbol(handle: usize, name: &CStr) -> Option<usize> {
    let r = with(None, |p| {
        let c = GuestBytes::c_str(name.to_bytes())?;
        // SAFETY: void* __loader_dlsym(void*, const char*, const void* caller); `handle` came from `load`.
        Ok(unsafe { p.guest.call(p.dlsym, handle, c.at, p.guest.exe_entry) })
    });
    r.ok().filter(|&f| f != 0)
}

/// A host-callable trampoline into guest function `f` taking a first argument of `kind`, with ART's JNI `shorty` (None for JNI_OnLoad / JNI_OnUnload). One per function, kind and shorty (`Active::down` deduplicates): ART may ask again for the same symbol.
/// A refusal is also counted with its reason (atl-diag `refused`, key `native-bridge.trampoline`): ART reports a null trampoline without calling `getError`, so the reason would otherwise reach no log.
pub fn trampoline(f: usize, kind: Down, shorty: Option<&[u8]>) -> Result<usize, String> {
    active().down(f, kind, shorty).inspect_err(|e| {
        atl_diag::count(atl_diag::REFUSED, "native-bridge.trampoline", Some(e));
    })
}

/// Whether `addr` lies in an object the guest linker loaded (guest `dladdr`); false without a guest.
/// Not narrowed to executable segments: ART asks only about method pointers `RegisterNatives` got.
pub fn is_guest_code(addr: usize) -> bool {
    with(None, |p| {
        // bionic Dl_info: dli_fname, dli_fbase, dli_sname, dli_saddr: four guest words, at most 64-bit ones; only the answer is read
        let info = GuestBytes::new(&[], 4 * 8)?;
        // SAFETY: int __loader_dladdr(const void*, Dl_info*); `info` is a Dl_info-sized buffer the guest can write.
        Ok(unsafe { p.guest.call(p.dladdr, addr, info.at, 0) } != 0)
    })
    .unwrap_or(false)
}

/// Guest `dlclose(handle)`: 0 or the linker's error.
pub fn unload(handle: usize) -> Result<(), String> {
    with(None, |p| {
        // SAFETY: int __loader_dlclose(void*); `handle` came from `load`.
        let rc = unsafe { p.guest.call(p.dlclose, handle, 0, 0) } as i32;
        if rc != 0 {
            return Err(format!("guest dlclose: {}", p.dlerror()));
        }
        Ok(())
    })
}
