//! v2probe: a lua51.dll proxy for Victoria 2 (install it in a spare copy of the game).
//!
//! Victoria 2 imports Lua from `lua51.dll`, which is itself a thin forwarder to the real
//! engine in `lua5.1.dll`. This DLL replaces that forwarder: 124 exports jump straight to
//! the real functions (see `generated.rs`), and four are wrapped to log what the engine does:
//!
//! * `luaL_openlibs`   – after the real call, keeps `io`/`os` reachable as `v2probe_io`/`v2probe_os`
//!   (the engine removes `io` and `os` later) and registers `v2probe_log(...)`.
//! * `luaL_loadbuffer` – logs every chunk the engine loads (name and size).
//! * `lua_getfield`    – counts global name lookups; new names are logged (reveals engine hooks).
//! * `lua_pcall`       – logs calls into Lua with duration, and any error message.
//!
//! Output: `v2probe_proxy.log` next to this DLL. 32-bit only (`v2game.exe` is 32-bit).

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]

#[allow(dead_code)]
mod generated;
mod cover;
mod hooks;
mod script;
mod watch;

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use generated::*;

type LuaState = *mut c_void;
type LuaCFunction = unsafe extern "C" fn(LuaState) -> c_int;
const LUA_GLOBALSINDEX: c_int = -10002;

// ---------------------------------------------------------------- Win32

unsafe extern "system" {
    fn LoadLibraryA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn GetModuleFileNameW(module: *mut c_void, buf: *mut u16, size: u32) -> u32;
}

/// Addresses of the real functions in lua5.1.dll, by export index. Filled in DllMain before
/// the game can call anything; read by the naked jump stubs in `generated.rs`.
pub static mut REAL: [usize; COUNT] = [0; COUNT];

static mut THIS_MODULE: usize = 0;
static mut MISSING: usize = 0;
/// Result of installing the exe hooks in DllMain, written to the log on first use.
static HOOK_STATUS: Mutex<Option<String>> = Mutex::new(None);

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(module: *mut c_void, reason: u32, _reserved: *mut c_void) -> c_int {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            THIS_MODULE = module as usize;
            let real = LoadLibraryA(c"lua5.1.dll".as_ptr().cast());
            if real.is_null() {
                return 0; // refuse to load rather than crash later
            }
            let table = &raw mut REAL;
            for (i, name) in NAMES.iter().enumerate() {
                let p = GetProcAddress(real, name.as_ptr());
                (*table)[i] = p as usize;
                if p.is_null() {
                    MISSING += 1;
                }
            }
            // Optional exe hook: addresses come from your own config file.
            let mut cfg = log_path();
            cfg.set_file_name("v2probe.txt");
            let status = match hooks::read_config(&cfg) {
                None => "no v2probe.txt: exe hooks off".to_string(),
                Some(c) => match hooks::install(&c) {
                    Ok(s) => s,
                    Err(e) => format!("exe hooks NOT installed: {e}"),
                },
            };
            if let Ok(mut g) = HOOK_STATUS.lock() {
                *g = Some(status);
            }
        }
    }
    1
}

unsafe fn real<T: Copy>(idx: usize) -> T {
    unsafe {
        let addr = REAL[idx];
        std::mem::transmute_copy::<usize, T>(&addr)
    }
}

// ---------------------------------------------------------------- logging

static START: OnceLock<Instant> = OnceLock::new();
static LOG: Mutex<Option<File>> = Mutex::new(None);

pub(crate) fn log_path() -> PathBuf {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(THIS_MODULE as *mut c_void, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let mut p = PathBuf::from(String::from_utf16_lossy(&buf[..n.min(buf.len())]));
    p.set_file_name("v2probe_proxy.log");
    p
}

pub(crate) fn log(msg: &str) {
    let start = START.get_or_init(Instant::now);
    let Ok(mut guard) = LOG.lock() else { return };
    if guard.is_none() {
        let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) else { return };
        let missing = unsafe { MISSING };
        let _ = writeln!(f, "==== v2probe lua51 proxy loaded; {COUNT} exports, {missing} missing in lua5.1.dll");
        if let Some(status) = HOOK_STATUS.lock().ok().and_then(|g| g.clone()) {
            let _ = writeln!(f, "==== {status}");
        }
        *guard = Some(f);
    }
    if let Some(f) = guard.as_mut() {
        let t = start.elapsed();
        let _ = writeln!(f, "[{:>7}.{:03}] {msg}", t.as_secs(), t.subsec_millis());
        let _ = f.flush();
    }
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return "<null>".into();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------- Lua helpers (real functions)

type FnGetField = unsafe extern "C" fn(LuaState, c_int, *const c_char);
type FnSetField = unsafe extern "C" fn(LuaState, c_int, *const c_char);
type FnPushCClosure = unsafe extern "C" fn(LuaState, LuaCFunction, c_int);
type FnGetTop = unsafe extern "C" fn(LuaState) -> c_int;
type FnType = unsafe extern "C" fn(LuaState, c_int) -> c_int;
type FnTypeName = unsafe extern "C" fn(LuaState, c_int) -> *const c_char;
type FnToLString = unsafe extern "C" fn(LuaState, c_int, *mut usize) -> *const c_char;

/// String form of a stack value: strings and numbers as text, anything else as its type name.
unsafe fn value_text(l: LuaState, idx: c_int) -> String {
    unsafe {
        let ty: FnType = real(IDX_lua_type);
        let t = ty(l, idx);
        const LUA_TNUMBER: c_int = 3;
        const LUA_TSTRING: c_int = 4;
        if t == LUA_TSTRING || t == LUA_TNUMBER {
            let tolstring: FnToLString = real(IDX_lua_tolstring);
            let mut len = 0usize;
            let p = tolstring(l, idx, &mut len);
            if p.is_null() {
                return String::new();
            }
            String::from_utf8_lossy(std::slice::from_raw_parts(p.cast::<u8>(), len)).into_owned()
        } else {
            let typename: FnTypeName = real(IDX_lua_typename);
            format!("<{}>", cstr(typename(l, t)))
        }
    }
}

/// `v2probe_log(...)` for Lua code: writes all arguments, tab-separated, to the proxy log.
unsafe extern "C" fn v2probe_log(l: LuaState) -> c_int {
    unsafe {
        let gettop: FnGetTop = real(IDX_lua_gettop);
        let n = gettop(l);
        let parts: Vec<String> = (1..=n).map(|i| value_text(l, i)).collect();
        log(&format!("lua: {}", parts.join("\t")));
    }
    0
}

// ---------------------------------------------------------------- hooks

#[unsafe(no_mangle)]
pub unsafe extern "C" fn luaL_openlibs(l: LuaState) {
    unsafe {
        let f: unsafe extern "C" fn(LuaState) = real(IDX_luaL_openlibs);
        f(l);
        let getfield: FnGetField = real(IDX_lua_getfield);
        let setfield: FnSetField = real(IDX_lua_setfield);
        let push: FnPushCClosure = real(IDX_lua_pushcclosure);
        // Keep the libraries the engine strips later reachable under our own names.
        getfield(l, LUA_GLOBALSINDEX, c"io".as_ptr());
        setfield(l, LUA_GLOBALSINDEX, c"v2probe_io".as_ptr());
        getfield(l, LUA_GLOBALSINDEX, c"os".as_ptr());
        setfield(l, LUA_GLOBALSINDEX, c"v2probe_os".as_ptr());
        push(l, v2probe_log, 0);
        setfield(l, LUA_GLOBALSINDEX, c"v2probe_log".as_ptr());
    }
    log(&format!("luaL_openlibs(L={l:p}): libraries opened; v2probe_io, v2probe_os, v2probe_log installed"));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn luaL_loadbuffer(l: LuaState, buff: *const c_char, size: usize, name: *const c_char) -> c_int {
    let r = unsafe {
        let f: unsafe extern "C" fn(LuaState, *const c_char, usize, *const c_char) -> c_int = real(IDX_luaL_loadbuffer);
        f(l, buff, size, name)
    };
    log(&format!("luaL_loadbuffer(L={l:p}, {size} bytes, name={}) -> {r}", unsafe { cstr(name) }));
    r
}

struct LookupStats {
    counts: HashMap<String, u64>,
    total: u64,
    summaries: u32,
}

static LOOKUPS: Mutex<Option<LookupStats>> = Mutex::new(None);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lua_getfield(l: LuaState, idx: c_int, k: *const c_char) {
    if idx == LUA_GLOBALSINDEX && !k.is_null() {
        let name = unsafe { CStr::from_ptr(k) }.to_string_lossy();
        if let Ok(mut guard) = LOOKUPS.lock() {
            let s = guard.get_or_insert_with(|| LookupStats { counts: HashMap::new(), total: 0, summaries: 0 });
            s.total += 1;
            let c = s.counts.entry(name.to_string()).or_insert(0);
            *c += 1;
            let first = *c == 1;
            let summary = s.total.is_multiple_of(5000) && s.summaries < 200;
            if summary {
                s.summaries += 1;
            }
            let report = summary.then(|| {
                let mut v: Vec<_> = s.counts.iter().collect();
                v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                let top: Vec<String> = v.iter().take(25).map(|(k, n)| format!("{k}={n}")).collect();
                format!("global lookups: {} total, {} names; top: {}", s.total, s.counts.len(), top.join(", "))
            });
            drop(guard);
            if first {
                log(&format!("global lookup (new): {name}"));
            }
            if let Some(r) = report {
                log(&r);
            }
        }
    }
    unsafe {
        let f: FnGetField = real(IDX_lua_getfield);
        f(l, idx, k);
    }
}

static PCALLS: Mutex<u64> = Mutex::new(0);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lua_pcall(l: LuaState, nargs: c_int, nresults: c_int, errfunc: c_int) -> c_int {
    let n = PCALLS.lock().map(|mut c| {
        *c += 1;
        *c
    });
    let t = Instant::now();
    let r = unsafe {
        let f: unsafe extern "C" fn(LuaState, c_int, c_int, c_int) -> c_int = real(IDX_lua_pcall);
        f(l, nargs, nresults, errfunc)
    };
    let n = n.unwrap_or(0);
    if r != 0 {
        log(&format!("lua_pcall #{n} (L={l:p}, nargs={nargs}) ERROR {r}: {}", unsafe { value_text(l, -1) }));
    } else if n <= 100 || n.is_multiple_of(1000) {
        log(&format!("lua_pcall #{n} (L={l:p}, nargs={nargs}, nresults={nresults}) ok in {:?}", t.elapsed()));
    }
    r
}
