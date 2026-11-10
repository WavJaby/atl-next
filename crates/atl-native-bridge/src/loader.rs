// SPDX-License-Identifier: MIT
//! The process's guest and the libraries ART asked it to load (NativeBridgeCallbacks `loadLibrary` / `unloadLibrary` / trampolines).
//! The guest starts on the first `loadLibrary`, not at `initialize`: a process without bridged libraries maps nothing (bridge1-step4-plan section 1).
//! Libraries go into the guest linker's default namespace, whose search path is the guest's own libraries plus the directory of every library loaded so far (`__loader_android_update_LD_LIBRARY_PATH`): an APK's `lib/<abi>/` holds the library's `DT_NEEDED` siblings (`libc++_shared.so`) and what it `dlopen`s by name.
//! Any thread: guest calls attach the calling thread first (engine thread.rs). The lock serialises the guest linker's entry points, as bionic's own `g_dl_mutex` would.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::engine::identity::Down;
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

/// None = not started; Some(Err) = the start failed (kept: a second start would map the guest twice).
static PROCESS: Mutex<Option<Result<Process, String>>> = Mutex::new(None);

fn linker_fn(g: &Guest, name: &str) -> Result<usize, String> {
    g.linker
        .symbol(name)
        .ok_or_else(|| format!("guest linker64 lacks {name}"))
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
        let c = CString::new(joined.into_encoded_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: void __loader_android_update_LD_LIBRARY_PATH(const char*): the linker copies the text.
        unsafe { self.guest.call(self.update_path, c.as_ptr() as usize, 0, 0) };
        Ok(())
    }
}

/// Loads `path` into the guest (the config is read only on the first call); the guest handle.
pub fn load(path: &Path, cfg: &dyn Fn() -> Config) -> Result<usize, String> {
    with(Some(cfg), |p| {
        if let Some(dir) = path.parent() {
            p.add_dir(dir)?;
        }
        let c = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: void* __loader_dlopen(const char*, int, const void* caller); the caller address puts the library in the main executable's (default) namespace.
        let h = unsafe {
            p.guest
                .call(p.dlopen, c.as_ptr() as usize, RTLD_NOW, p.guest.exe_entry)
        };
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
        // SAFETY: void* __loader_dlsym(void*, const char*, const void* caller); `handle` came from `load`.
        Ok(unsafe {
            p.guest
                .call(p.dlsym, handle, name.as_ptr() as usize, p.guest.exe_entry)
        })
    });
    r.ok().filter(|&f| f != 0)
}

/// A host-callable trampoline into guest function `f` taking a first argument of `kind`, with ART's JNI `shorty` (None for JNI_OnLoad / JNI_OnUnload). One per function, kind and shorty: ART may ask again for the same symbol.
pub fn trampoline(f: usize, kind: Down, shorty: Option<&[u8]>) -> Result<usize, String> {
    type Stub = (usize, Down, Option<Vec<u8>>, usize);
    static STUBS: Mutex<Vec<Stub>> = Mutex::new(Vec::new());
    let mut stubs = STUBS
        .lock()
        .map_err(|_| "trampolines poisoned".to_owned())?;
    if let Some(stub) = stubs
        .iter()
        .find(|s| s.0 == f && s.1 == kind && s.2.as_deref() == shorty)
    {
        return Ok(stub.3);
    }
    let t = active().down(f, kind, shorty)?;
    stubs.push((f, kind, shorty.map(<[u8]>::to_vec), t));
    Ok(t)
}

/// Whether `addr` lies in an object the guest linker loaded (guest `dladdr`); false without a guest.
/// Not narrowed to executable segments: ART asks only about method pointers `RegisterNatives` got.
pub fn is_guest_code(addr: usize) -> bool {
    with(None, |p| {
        // bionic Dl_info: dli_fname, dli_fbase, dli_sname, dli_saddr
        let mut info = [0usize; 4];
        // SAFETY: int __loader_dladdr(const void*, Dl_info*); `info` is a Dl_info-sized buffer.
        Ok(unsafe { p.guest.call(p.dladdr, addr, info.as_mut_ptr() as usize, 0) } != 0)
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
