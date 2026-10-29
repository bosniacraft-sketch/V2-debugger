//! Scripting: a Lua script you write drives the measurements, so a new experiment is a script
//! edit instead of a rebuild of this DLL.
//!
//! On the first daily tick the proxy creates its own Lua state in the real `lua5.1.dll` (separate
//! from the game's, with the full standard libraries) and loads `v2probe_script.lua` from the
//! folder of this DLL. The file is reloaded whenever it changes on disk, also while the game runs.
//! Each day the proxy calls these optional global functions of the script (`raw_date` is the
//! value at `date_offset`, or nil when that key is not configured):
//!
//!   before_tick(n, raw_date)           before the game's own daily tick runs (date = old day)
//!   on_tick(n, raw_date)               after it
//!   on_idle(n, raw_date)               about ten times a second from a background thread, also
//!                                      while the game is paused (n = ticks so far). The game may
//!                                      be mid-update: read with care.
//!
//! The script sees one global table, `gm`. Every address is a plain number; every read is
//! range-checked against the process's committed memory and returns nil when it is not readable.
//!
//!   gm.base()  gm.gs()                 exe load address; game state pointer (0 when none)
//!   gm.cfg                             table of all numeric keys of v2probe.txt
//!   gm.u8 u16 u32 i8 i16 i32 i64 f32 f64 (addr)   one value (i64 as a Lua number, exact to 2^53)
//!   gm.ptr(addr)                       same as u32
//!   gm.bytes(addr, n)                  n raw bytes as a string, for io.write into a binary dump
//!   gm.hex(addr, n)                    the same as hex text
//!   gm.cstr(addr [, max])              zero-terminated text (max 256 by default)
//!   gm.str(addr)                       an MSVC std::string object at addr
//!   gm.readable(addr, n)               true when the whole range can be read
//!   gm.heapscan()                      lists every heap block of the process; returns count, bytes
//!   gm.heapsize(addr)                  size of the block that starts at addr (as of the last scan), or nil
//!   gm.heapblock(addr)                 start, size of the block that contains addr, or nil
//!   gm.tick()  gm.date()               tick number; raw date (nil when not configured)
//!   gm.dir()                           folder of this DLL, with a trailing backslash
//!   gm.log(...)                        writes to v2probe_proxy.log
//!   gm.clock()                         seconds since the DLL was loaded
//!   gm.fopen(path [, append])          opens a file for binary writing; returns a handle, or nil + error
//!   gm.fwrite(handle, ...)             writes its string arguments; gm.fclose(handle)
//!   gm.readfile(path)                  a whole file as a string, or nil
//!                                      (the game's Lua build has no `io` or `os` library)
//!   gm.quit()                          ends the game process once this tick's work is done
//!
//! Writes exist only when v2probe.txt has `allow_write = 1`, and each one is logged:
//!
//!   gm.w8 w16 w32 w64 (addr, value)    returns true when written
//!   gm.wbytes(addr, string)
//!
//! Write-watch (see `watch.rs`; call from before_tick/on_tick, it watches the calling thread):
//!
//!   gm.watch(addr, len [, reads])      reports code that writes (or also reads) addr; slot 0-3 or nil
//!   gm.unwatch([slot])                 clears one slot, or all
//!   gm.watch_hits()                    array of {slot=, rva=, count=, callers="rva rva ..."}; clears it
//!
//! Coverage (see `cover.rs`): which listed functions run, and in what order:
//!
//!   gm.cover_load(path)                reads a list of function RVAs (hex, one per line); returns the count
//!   gm.cover_arm()  gm.cover_disarm()  start and stop recording first calls
//!   gm.cover_hits()                    text: one `order rva thread_id` line per function hit
//!
//! This file holds no addresses and no game data.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CString};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::generated::*;
use crate::{log, real, value_text, LuaCFunction, LuaState, LUA_GLOBALSINDEX};

const SCRIPT_FILE: &str = "v2probe_script.lua";
const LUA_TFUNCTION: c_int = 6;
/// Largest single read or write a script may ask for.
const MAX_RANGE: usize = 64 << 20;

// ---------------------------------------------------------------- Win32

#[repr(C)]
struct MemoryBasicInformation {
    base: usize,
    alloc_base: usize,
    alloc_protect: u32,
    #[cfg(target_pointer_width = "64")]
    partition: u32,
    region_size: usize,
    state: u32,
    protect: u32,
    kind: u32,
    #[cfg(target_pointer_width = "64")]
    pad: u32,
}

/// PROCESS_HEAP_ENTRY: one step of HeapWalk. The trailing union is two ints and two pointers.
#[repr(C)]
struct ProcessHeapEntry {
    data: *mut c_void,
    size: u32,
    overhead: u8,
    region: u8,
    flags: u16,
    detail: [u32; 2],
    blocks: [usize; 2],
}

const PROCESS_HEAP_ENTRY_BUSY: u16 = 0x0004;

unsafe extern "system" {
    fn VirtualQuery(addr: *const c_void, info: *mut MemoryBasicInformation, len: usize) -> usize;
    fn GetProcessHeaps(count: u32, heaps: *mut *mut c_void) -> u32;
    fn HeapLock(heap: *mut c_void) -> i32;
    fn HeapUnlock(heap: *mut c_void) -> i32;
    fn HeapWalk(heap: *mut c_void, entry: *mut ProcessHeapEntry) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
}

const MEM_COMMIT: u32 = 0x1000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_GUARD: u32 = 0x100;
const PAGE_WRITABLE: u32 = 0x04 | 0x08 | 0x40 | 0x80;

/// True when every page of `addr..addr+len` is committed and readable (and writable if asked).
fn accessible(addr: usize, len: usize, write: bool) -> bool {
    if addr == 0 || len == 0 || len > MAX_RANGE {
        return false;
    }
    let Some(end) = addr.checked_add(len) else { return false };
    let mut at = addr;
    while at < end {
        let mut info = std::mem::MaybeUninit::<MemoryBasicInformation>::zeroed();
        let got = unsafe { VirtualQuery(at as *const c_void, info.as_mut_ptr(), size_of::<MemoryBasicInformation>()) };
        if got == 0 {
            return false;
        }
        let info = unsafe { info.assume_init() };
        if !region_ok(info.state, info.protect, write) {
            return false;
        }
        let Some(next) = info.base.checked_add(info.region_size) else { return false };
        if next <= at {
            return false;
        }
        at = next;
    }
    true
}

fn region_ok(state: u32, protect: u32, write: bool) -> bool {
    state == MEM_COMMIT && protect != 0 && protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 && (!write || protect & PAGE_WRITABLE != 0)
}

// ---------------------------------------------------------------- state

static STATE: AtomicUsize = AtomicUsize::new(0);
static ALLOW_WRITE: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);
static CONFIG: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);
/// Modification time of the script when last loaded (`None` = no file then).
static LOADED: Mutex<Option<Option<SystemTime>>> = Mutex::new(None);
/// Raw date of the tick being handled, plus one (0 = none).
static DATE: AtomicUsize = AtomicUsize::new(0);

/// Keeps the hook configuration for `gm.cfg`. Called once from DllMain; touches no files.
pub fn configure(config: &HashMap<String, usize>) -> &'static str {
    let write = config.get("allow_write").is_some_and(|v| *v != 0);
    ALLOW_WRITE.store(write, Ordering::Relaxed);
    if let Ok(mut g) = CONFIG.lock() {
        *g = Some(config.clone());
    }
    if write { ", script (writes allowed)" } else { ", script (read-only)" }
}

fn script_path() -> std::path::PathBuf {
    let mut p = crate::log_path();
    p.set_file_name(SCRIPT_FILE);
    p
}

// ---------------------------------------------------------------- Lua plumbing

type FnVoid = unsafe extern "C" fn(LuaState);
type FnNewState = unsafe extern "C" fn() -> LuaState;
type FnLoadFile = unsafe extern "C" fn(LuaState, *const c_char) -> c_int;
type FnPCall = unsafe extern "C" fn(LuaState, c_int, c_int, c_int) -> c_int;
type FnField = unsafe extern "C" fn(LuaState, c_int, *const c_char);
type FnPushCClosure = unsafe extern "C" fn(LuaState, LuaCFunction, c_int);
type FnPushNumber = unsafe extern "C" fn(LuaState, f64);
type FnPushLString = unsafe extern "C" fn(LuaState, *const c_char, usize);
type FnPushBool = unsafe extern "C" fn(LuaState, c_int);
type FnToNumber = unsafe extern "C" fn(LuaState, c_int) -> f64;
type FnToLString = unsafe extern "C" fn(LuaState, c_int, *mut usize) -> *const c_char;
type FnIndex = unsafe extern "C" fn(LuaState, c_int);
type FnInt = unsafe extern "C" fn(LuaState) -> c_int;
type FnIntAt = unsafe extern "C" fn(LuaState, c_int) -> c_int;
type FnCreateTable = unsafe extern "C" fn(LuaState, c_int, c_int);

unsafe fn push_num(l: LuaState, v: f64) {
    unsafe { real::<FnPushNumber>(IDX_lua_pushnumber)(l, v) }
}
unsafe fn push_nil(l: LuaState) {
    unsafe { real::<FnVoid>(IDX_lua_pushnil)(l) }
}
unsafe fn push_bool(l: LuaState, v: bool) {
    unsafe { real::<FnPushBool>(IDX_lua_pushboolean)(l, v as c_int) }
}
unsafe fn push_bytes(l: LuaState, b: &[u8]) {
    unsafe { real::<FnPushLString>(IDX_lua_pushlstring)(l, b.as_ptr().cast(), b.len()) }
}
unsafe fn top(l: LuaState) -> c_int {
    unsafe { real::<FnInt>(IDX_lua_gettop)(l) }
}
unsafe fn set_top(l: LuaState, n: c_int) {
    unsafe { real::<FnIndex>(IDX_lua_settop)(l, n) }
}
/// Argument `i` as an address or size; 0 when missing or negative.
unsafe fn arg(l: LuaState, i: c_int) -> usize {
    let v = unsafe { real::<FnToNumber>(IDX_lua_tonumber)(l, i) };
    if v.is_finite() && v > 0.0 { v as u64 as usize } else { 0 }
}
unsafe fn arg_bytes<'a>(l: LuaState, i: c_int) -> &'a [u8] {
    let mut len = 0usize;
    let p = unsafe { real::<FnToLString>(IDX_lua_tolstring)(l, i, &mut len) };
    if p.is_null() { &[] } else { unsafe { std::slice::from_raw_parts(p.cast(), len) } }
}

/// The bytes at `addr`, or None when any of them is not readable.
unsafe fn mem<'a>(addr: usize, len: usize) -> Option<&'a [u8]> {
    accessible(addr, len, false).then(|| unsafe { std::slice::from_raw_parts(addr as *const u8, len) })
}

macro_rules! reader {
    ($name:ident, $ty:ty) => {
        unsafe extern "C" fn $name(l: LuaState) -> c_int {
            unsafe {
                match mem(arg(l, 1), size_of::<$ty>()) {
                    Some(b) => push_num(l, std::ptr::read_unaligned(b.as_ptr().cast::<$ty>()) as f64),
                    None => push_nil(l),
                }
            }
            1
        }
    };
}
reader!(gm_u8, u8);
reader!(gm_u16, u16);
reader!(gm_u32, u32);
reader!(gm_i8, i8);
reader!(gm_i16, i16);
reader!(gm_i32, i32);
reader!(gm_i64, i64);
reader!(gm_f32, f32);
reader!(gm_f64, f64);

macro_rules! writer {
    ($name:ident, $ty:ty, $via:ty) => {
        unsafe extern "C" fn $name(l: LuaState) -> c_int {
            unsafe {
                let v = real::<FnToNumber>(IDX_lua_tonumber)(l, 2) as $via as $ty;
                let ok = write(arg(l, 1), &v.to_le_bytes());
                push_bool(l, ok);
            }
            1
        }
    };
}
writer!(gm_w8, u8, i64);
writer!(gm_w16, u16, i64);
writer!(gm_w32, u32, i64);
writer!(gm_w64, i64, i64);

unsafe fn write(addr: usize, bytes: &[u8]) -> bool {
    if !ALLOW_WRITE.load(Ordering::Relaxed) {
        log(&format!("script write to {addr:#x} refused: allow_write is not set in v2probe.txt"));
        return false;
    }
    if !accessible(addr, bytes.len(), true) {
        log(&format!("script write to {addr:#x} ({} bytes) refused: not writable memory", bytes.len()));
        return false;
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
    let shown: String = bytes.iter().take(16).map(|b| format!("{b:02x}")).collect();
    log(&format!("script WRITE {addr:#x} <- {shown}{} ({} bytes)", if bytes.len() > 16 { "..." } else { "" }, bytes.len()));
    true
}

unsafe extern "C" fn gm_wbytes(l: LuaState) -> c_int {
    unsafe {
        let ok = write(arg(l, 1), arg_bytes(l, 2));
        push_bool(l, ok);
    }
    1
}

unsafe extern "C" fn gm_bytes(l: LuaState) -> c_int {
    unsafe {
        match mem(arg(l, 1), arg(l, 2)) {
            Some(b) => push_bytes(l, b),
            None => push_nil(l),
        }
    }
    1
}

unsafe extern "C" fn gm_hex(l: LuaState) -> c_int {
    unsafe {
        match mem(arg(l, 1), arg(l, 2)) {
            Some(b) => push_bytes(l, to_hex(b).as_bytes()),
            None => push_nil(l),
        }
    }
    1
}

fn to_hex(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(DIGITS[(x >> 4) as usize] as char);
        s.push(DIGITS[(x & 15) as usize] as char);
    }
    s
}

/// Zero-terminated text at `addr`, at most `max` bytes, stopping at the end of readable memory.
unsafe fn c_text<'a>(addr: usize, max: usize) -> Option<&'a [u8]> {
    let mut len = 0;
    while len < max {
        // Check a page at a time, so text near the end of a region is still returned.
        let step = (0x1000 - ((addr + len) & 0xfff)).min(max - len);
        let chunk = unsafe { mem(addr + len, step) }?;
        if let Some(z) = chunk.iter().position(|b| *b == 0) {
            return Some(unsafe { std::slice::from_raw_parts(addr as *const u8, len + z) });
        }
        len += step;
    }
    Some(unsafe { std::slice::from_raw_parts(addr as *const u8, max) })
}

unsafe extern "C" fn gm_cstr(l: LuaState) -> c_int {
    unsafe {
        let max = match arg(l, 2) {
            0 => 256,
            n => n.min(MAX_RANGE),
        };
        match c_text(arg(l, 1), max) {
            Some(b) => push_bytes(l, b),
            None => push_nil(l),
        }
    }
    1
}

/// MSVC std::string: 16 bytes of inline text or a pointer, then length and capacity.
unsafe extern "C" fn gm_str(l: LuaState) -> c_int {
    unsafe {
        let obj = arg(l, 1);
        let text = mem(obj, 24).and_then(|b| {
            let word = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize;
            let (len, cap) = (word(16), word(20));
            if len > cap || len > 0x10000 {
                return None;
            }
            mem(if cap > 15 { word(0) } else { obj }, len.max(1)).map(|t| &t[..len])
        });
        match text {
            Some(t) => push_bytes(l, t),
            None => push_nil(l),
        }
    }
    1
}

unsafe extern "C" fn gm_readable(l: LuaState) -> c_int {
    unsafe { push_bool(l, accessible(arg(l, 1), arg(l, 2).max(1), false)) };
    1
}

/// Every allocated heap block of the process as (address, size), sorted by address. Filled by
/// `gm.heapscan()`. Asking Windows about one arbitrary pointer (HeapValidate/HeapSize) can fault
/// inside ntdll when the pointer is not a block start, so the blocks are listed by walking the
/// heaps instead, and lookups only search this table.
static HEAP_BLOCKS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// `gm.heapscan()`: lists all heap blocks (each heap locked while walked). Returns the number
/// of blocks and their total size.
unsafe extern "C" fn gm_heapscan(l: LuaState) -> c_int {
    let mut blocks = Vec::new();
    unsafe {
        let mut heaps = [std::ptr::null_mut::<c_void>(); 256];
        let n = (GetProcessHeaps(heaps.len() as u32, heaps.as_mut_ptr()) as usize).min(heaps.len());
        for heap in &heaps[..n] {
            if HeapLock(*heap) == 0 {
                continue; // a heap without locking cannot be walked safely
            }
            let mut entry: ProcessHeapEntry = std::mem::zeroed();
            while HeapWalk(*heap, &mut entry) != 0 {
                if entry.flags & PROCESS_HEAP_ENTRY_BUSY != 0 && !entry.data.is_null() {
                    blocks.push((entry.data as usize, entry.size as usize));
                }
            }
            HeapUnlock(*heap);
        }
    }
    blocks.sort_unstable();
    let total: f64 = blocks.iter().map(|b| b.1 as f64).sum();
    let count = blocks.len();
    if let Ok(mut g) = HEAP_BLOCKS.lock() {
        *g = blocks;
    }
    unsafe {
        push_num(l, count as f64);
        push_num(l, total);
    }
    2
}

/// The block that starts exactly at `addr` (`exact`) or contains it, from the last heap scan.
fn find_block(blocks: &[(usize, usize)], addr: usize, exact: bool) -> Option<(usize, usize)> {
    let i = blocks.partition_point(|b| b.0 <= addr).checked_sub(1)?;
    let (start, size) = blocks[i];
    let hit = if exact { start == addr } else { addr - start < size.max(1) };
    hit.then_some((start, size))
}

/// `gm.heapsize(addr)`: size of the heap block starting exactly at `addr`, or nil.
unsafe extern "C" fn gm_heapsize(l: LuaState) -> c_int {
    let found = HEAP_BLOCKS.lock().ok().and_then(|b| find_block(&b, unsafe { arg(l, 1) }, true));
    unsafe {
        match found {
            Some((_, size)) => push_num(l, size as f64),
            None => push_nil(l),
        }
    }
    1
}

/// `gm.heapblock(addr)`: start and size of the heap block that contains `addr`, or nil.
unsafe extern "C" fn gm_heapblock(l: LuaState) -> c_int {
    let found = HEAP_BLOCKS.lock().ok().and_then(|b| find_block(&b, unsafe { arg(l, 1) }, false));
    unsafe {
        match found {
            Some((start, size)) => {
                push_num(l, start as f64);
                push_num(l, size as f64);
                2
            }
            None => {
                push_nil(l);
                1
            }
        }
    }
}

unsafe extern "C" fn gm_watch(l: LuaState) -> c_int {
    unsafe {
        let reads = real::<FnIntAt>(IDX_lua_toboolean)(l, 3) != 0;
        match crate::watch::watch(arg(l, 1), arg(l, 2), reads) {
            Some(slot) => push_num(l, slot as f64),
            None => push_nil(l),
        }
    }
    1
}

unsafe extern "C" fn gm_unwatch(l: LuaState) -> c_int {
    unsafe {
        let slot = (top(l) >= 1).then(|| arg(l, 1));
        crate::watch::unwatch(slot);
    }
    0
}

unsafe extern "C" fn gm_watch_hits(l: LuaState) -> c_int {
    let base = crate::hooks::exe_base();
    let hits = crate::watch::take_hits();
    unsafe {
        let set: FnField = real(IDX_lua_setfield);
        real::<FnCreateTable>(IDX_lua_createtable)(l, hits.len() as c_int, 0);
        for (i, (slot, at, count, callers)) in hits.iter().enumerate() {
            real::<FnCreateTable>(IDX_lua_createtable)(l, 0, 4);
            push_num(l, *slot as f64);
            set(l, -2, c"slot".as_ptr());
            push_num(l, at.wrapping_sub(base) as f64);
            set(l, -2, c"rva".as_ptr());
            push_num(l, *count as f64);
            set(l, -2, c"count".as_ptr());
            let text: Vec<String> = callers.iter().map(|c| format!("{:x}", c.wrapping_sub(base))).collect();
            push_bytes(l, text.join(" ").as_bytes());
            set(l, -2, c"callers".as_ptr());
            real::<unsafe extern "C" fn(LuaState, c_int, c_int)>(IDX_lua_rawseti)(l, -2, i as c_int + 1);
        }
    }
    1
}

unsafe extern "C" fn gm_cover_load(l: LuaState) -> c_int {
    unsafe {
        let path = String::from_utf8_lossy(arg_bytes(l, 1)).into_owned();
        match std::fs::read_to_string(&path) {
            Ok(text) => push_num(l, crate::cover::load(crate::hooks::exe_base(), &crate::cover::parse_list(&text)) as f64),
            Err(_) => push_nil(l),
        }
    }
    1
}

unsafe extern "C" fn gm_cover_arm(l: LuaState) -> c_int {
    unsafe {
        let n = crate::cover::arm();
        log(&format!("script: coverage armed on {n} functions"));
        push_num(l, n as f64);
    }
    1
}

unsafe extern "C" fn gm_cover_disarm(l: LuaState) -> c_int {
    unsafe { push_num(l, crate::cover::disarm() as f64) };
    1
}

unsafe extern "C" fn gm_cover_hits(l: LuaState) -> c_int {
    unsafe { push_bytes(l, crate::cover::hits(crate::hooks::exe_base()).as_bytes()) };
    1
}

unsafe extern "C" fn gm_base(l: LuaState) -> c_int {
    unsafe { push_num(l, crate::hooks::exe_base() as f64) };
    1
}

unsafe extern "C" fn gm_gs(l: LuaState) -> c_int {
    unsafe { push_num(l, crate::hooks::game_state() as f64) };
    1
}

unsafe extern "C" fn gm_tick(l: LuaState) -> c_int {
    unsafe { push_num(l, crate::hooks::ticks() as f64) };
    1
}

unsafe extern "C" fn gm_date(l: LuaState) -> c_int {
    match DATE.load(Ordering::Relaxed) {
        0 => unsafe { real::<FnVoid>(IDX_lua_pushnil)(l) },
        d => unsafe { push_num(l, (d - 1) as u32 as i32 as f64) },
    }
    1
}

unsafe extern "C" fn gm_dir(l: LuaState) -> c_int {
    let mut p = crate::log_path();
    p.pop();
    let mut s = p.to_string_lossy().into_owned();
    s.push('\\');
    unsafe { push_bytes(l, s.as_bytes()) };
    1
}

unsafe extern "C" fn gm_log(l: LuaState) -> c_int {
    unsafe {
        let parts: Vec<String> = (1..=top(l)).map(|i| value_text(l, i)).collect();
        log(&format!("script: {}", parts.join("\t")));
    }
    0
}

static CLOCK: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

unsafe extern "C" fn gm_clock(l: LuaState) -> c_int {
    let t = CLOCK.get_or_init(std::time::Instant::now).elapsed().as_secs_f64();
    unsafe { push_num(l, t) };
    1
}

/// Files opened by the script; a handle is the slot number + 1.
static FILES: Mutex<Vec<Option<std::io::BufWriter<std::fs::File>>>> = Mutex::new(Vec::new());

unsafe extern "C" fn gm_fopen(l: LuaState) -> c_int {
    unsafe {
        let path = String::from_utf8_lossy(arg_bytes(l, 1)).into_owned();
        let append = real::<FnIntAt>(IDX_lua_toboolean)(l, 2) != 0;
        let opened = std::fs::OpenOptions::new().create(true).write(true).append(append).truncate(!append).open(&path);
        match (opened, FILES.lock()) {
            (Ok(f), Ok(mut files)) => {
                let slot = files.iter().position(Option::is_none).unwrap_or_else(|| {
                    files.push(None);
                    files.len() - 1
                });
                files[slot] = Some(std::io::BufWriter::with_capacity(1 << 20, f));
                push_num(l, (slot + 1) as f64);
                1
            }
            (Err(e), _) => {
                push_nil(l);
                push_bytes(l, e.to_string().as_bytes());
                2
            }
            _ => {
                push_nil(l);
                1
            }
        }
    }
}

unsafe extern "C" fn gm_fwrite(l: LuaState) -> c_int {
    use std::io::Write;
    unsafe {
        let handle = arg(l, 1);
        let mut ok = false;
        if let Ok(mut files) = FILES.lock()
            && let Some(Some(f)) = handle.checked_sub(1).and_then(|i| files.get_mut(i))
        {
            ok = (2..=top(l)).all(|i| f.write_all(arg_bytes(l, i)).is_ok());
        }
        push_bool(l, ok);
    }
    1
}

unsafe extern "C" fn gm_fclose(l: LuaState) -> c_int {
    use std::io::Write;
    unsafe {
        let handle = arg(l, 1);
        let mut ok = false;
        if let Ok(mut files) = FILES.lock()
            && let Some(mut f) = handle.checked_sub(1).and_then(|i| files.get_mut(i)).and_then(Option::take)
        {
            ok = f.flush().is_ok();
        }
        push_bool(l, ok);
    }
    1
}

unsafe extern "C" fn gm_readfile(l: LuaState) -> c_int {
    unsafe {
        match std::fs::read(String::from_utf8_lossy(arg_bytes(l, 1)).as_ref()) {
            Ok(data) => push_bytes(l, &data),
            Err(_) => push_nil(l),
        }
    }
    1
}

unsafe extern "C" fn gm_quit(_l: LuaState) -> c_int {
    QUIT.store(true, Ordering::Relaxed);
    0
}

/// Creates the script's Lua state with the standard libraries and the `gm` table.
unsafe fn new_state() -> LuaState {
    unsafe {
        let l = real::<FnNewState>(IDX_luaL_newstate)();
        if l.is_null() {
            return l;
        }
        real::<FnVoid>(IDX_luaL_openlibs)(l);
        let functions: [(&std::ffi::CStr, LuaCFunction); 41] = [
            (c"u8", gm_u8),
            (c"u16", gm_u16),
            (c"u32", gm_u32),
            (c"ptr", gm_u32),
            (c"i8", gm_i8),
            (c"i16", gm_i16),
            (c"i32", gm_i32),
            (c"i64", gm_i64),
            (c"f32", gm_f32),
            (c"f64", gm_f64),
            (c"bytes", gm_bytes),
            (c"hex", gm_hex),
            (c"cstr", gm_cstr),
            (c"str", gm_str),
            (c"readable", gm_readable),
            (c"cover_load", gm_cover_load),
            (c"cover_arm", gm_cover_arm),
            (c"cover_disarm", gm_cover_disarm),
            (c"cover_hits", gm_cover_hits),
            (c"watch", gm_watch),
            (c"unwatch", gm_unwatch),
            (c"watch_hits", gm_watch_hits),
            (c"heapscan", gm_heapscan),
            (c"heapsize", gm_heapsize),
            (c"heapblock", gm_heapblock),
            (c"base", gm_base),
            (c"gs", gm_gs),
            (c"tick", gm_tick),
            (c"date", gm_date),
            (c"dir", gm_dir),
            (c"log", gm_log),
            (c"clock", gm_clock),
            (c"fopen", gm_fopen),
            (c"fwrite", gm_fwrite),
            (c"fclose", gm_fclose),
            (c"readfile", gm_readfile),
            (c"quit", gm_quit),
            (c"w8", gm_w8),
            (c"w16", gm_w16),
            (c"w32", gm_w32),
            (c"w64", gm_w64),
        ];
        let set: FnField = real(IDX_lua_setfield);
        let push: FnPushCClosure = real(IDX_lua_pushcclosure);
        real::<FnCreateTable>(IDX_lua_createtable)(l, 0, 48);
        for (name, f) in functions {
            push(l, f, 0);
            set(l, -2, name.as_ptr());
        }
        push(l, gm_wbytes, 0);
        set(l, -2, c"wbytes".as_ptr());
        // gm.cfg: the numeric keys of v2probe.txt.
        real::<FnCreateTable>(IDX_lua_createtable)(l, 0, 64);
        if let Ok(guard) = CONFIG.lock() {
            for (k, v) in guard.iter().flatten() {
                if let Ok(key) = CString::new(k.as_str()) {
                    push_num(l, *v as f64);
                    set(l, -2, key.as_ptr());
                }
            }
        }
        set(l, -2, c"cfg".as_ptr());
        set(l, LUA_GLOBALSINDEX, c"gm".as_ptr());
        l
    }
}

/// Loads the script when it is new or changed on disk. Returns the Lua state, or null when
/// there is no script.
unsafe fn ensure_loaded() -> LuaState {
    let path = script_path();
    let now = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let Ok(mut loaded) = LOADED.lock() else { return std::ptr::null_mut() };
    let mut l = STATE.load(Ordering::Relaxed) as LuaState;
    if *loaded == Some(now) {
        return if now.is_some() { l } else { std::ptr::null_mut() };
    }
    let first = loaded.is_none();
    *loaded = Some(now);
    if now.is_none() {
        log(&format!("script: no {SCRIPT_FILE}; scripting idle"));
        return std::ptr::null_mut();
    }
    unsafe {
        if l.is_null() {
            l = new_state();
            if l.is_null() {
                log("script: could not create a Lua state");
                return l;
            }
            STATE.store(l as usize, Ordering::Relaxed);
        }
        let Ok(file) = CString::new(path.to_string_lossy().as_bytes()) else { return std::ptr::null_mut() };
        let mut status = real::<FnLoadFile>(IDX_luaL_loadfile)(l, file.as_ptr());
        if status == 0 {
            status = real::<FnPCall>(IDX_lua_pcall)(l, 0, 0, 0);
        }
        if status != 0 {
            log(&format!("script: {SCRIPT_FILE} failed to load: {}", value_text(l, -1)));
            set_top(l, 0);
        } else {
            log(&format!("script: {SCRIPT_FILE} {}", if first { "loaded" } else { "reloaded after a change" }));
        }
    }
    l
}

/// Runs `f` with the x87 FPU set to double precision, then restores the previous setting.
/// The game's Direct3D device switches the FPU to single precision, which would round every
/// Lua number above 2^24 (all addresses) inside the script.
#[cfg(target_arch = "x86")]
unsafe fn with_double_precision<R>(f: impl FnOnce() -> R) -> R {
    let mut saved: u16 = 0;
    unsafe {
        core::arch::asm!("fnstcw word ptr [{p}]", p = in(reg) &mut saved, options(nostack));
        let wanted: u16 = (saved & !0x0300) | 0x0200;
        core::arch::asm!("fldcw word ptr [{p}]", p = in(reg) &wanted, options(nostack));
        let result = f();
        core::arch::asm!("fldcw word ptr [{p}]", p = in(reg) &saved, options(nostack));
        result
    }
}
#[cfg(not(target_arch = "x86"))]
unsafe fn with_double_precision<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Calls the script's global function `name(n, raw_date)` if it exists (`raw_date` is nil when
/// `date_offset` is not configured).
pub unsafe fn call(name: &std::ffi::CStr, n: u64, date: Option<i32>) {
    // One caller at a time: the tick thread and the idle thread share the Lua state.
    let _turn = CALL_LOCK.lock();
    DATE.store(date.map_or(0, |d| d as u32 as usize + 1), Ordering::Relaxed);
    unsafe { with_double_precision(|| call_inner(name, n, date)) }
}

unsafe fn call_inner(name: &std::ffi::CStr, n: u64, date: Option<i32>) {
    unsafe {
        let l = ensure_loaded();
        if l.is_null() {
            return;
        }
        real::<FnField>(IDX_lua_getfield)(l, LUA_GLOBALSINDEX, name.as_ptr());
        if real::<FnIntAt>(IDX_lua_type)(l, -1) != LUA_TFUNCTION {
            set_top(l, 0);
            return;
        }
        push_num(l, n as f64);
        match date {
            Some(d) => push_num(l, d as f64),
            None => real::<FnVoid>(IDX_lua_pushnil)(l),
        }
        if real::<FnPCall>(IDX_lua_pcall)(l, 2, 0, 0) != 0 {
            log(&format!("script: {} failed: {}", name.to_string_lossy(), value_text(l, -1)));
        }
        set_top(l, 0);
    }
}

static CALL_LOCK: Mutex<()> = Mutex::new(());
const IDLE_PERIOD: std::time::Duration = std::time::Duration::from_millis(100);

/// Starts the thread behind `on_idle`. Called from DllMain: the thread only starts running
/// once the loader is done, and does nothing until the game state is set (or, without
/// `gamestate_ptr`, until the first tick).
pub fn start_idle_thread() {
    let _ = std::thread::Builder::new().name("v2probe-idle".into()).spawn(|| loop {
        std::thread::sleep(IDLE_PERIOD);
        if crate::hooks::game_state() != 0 || crate::hooks::ticks() > 0 {
            unsafe { call(c"on_idle", crate::hooks::ticks(), crate::hooks::raw_date()) };
            quit_if_requested();
        }
    });
}

/// Ends the game if the script asked for it. Called at the very end of a tick.
pub fn quit_if_requested() {
    if QUIT.load(Ordering::Relaxed) {
        log("script: gm.quit() - ending the game process");
        unsafe { TerminateProcess(GetCurrentProcess(), 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_text() {
        assert_eq!(to_hex(&[0x00, 0x7f, 0xa5, 0xff]), "007fa5ff");
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn finds_heap_blocks() {
        let blocks = [(100, 16), (200, 0), (300, 8)];
        assert_eq!(find_block(&blocks, 100, true), Some((100, 16)));
        assert_eq!(find_block(&blocks, 104, true), None);
        assert_eq!(find_block(&blocks, 115, false), Some((100, 16)));
        assert_eq!(find_block(&blocks, 116, false), None);
        assert_eq!(find_block(&blocks, 200, true), Some((200, 0)));
        assert_eq!(find_block(&blocks, 99, false), None);
        assert_eq!(find_block(&[], 5, false), None);
    }

    #[test]
    fn region_rules() {
        const RW: u32 = 0x04;
        const RO: u32 = 0x02;
        assert!(region_ok(MEM_COMMIT, RO, false));
        assert!(!region_ok(MEM_COMMIT, RO, true), "read-only memory is not writable");
        assert!(region_ok(MEM_COMMIT, RW, true));
        assert!(!region_ok(0x2000, RW, false), "reserved, not committed");
        assert!(!region_ok(MEM_COMMIT, RW | PAGE_GUARD, false));
        assert!(!region_ok(MEM_COMMIT, PAGE_NOACCESS, false));
    }

    /// End-to-end check of the `gm` functions in a real Lua state. Needs the 32-bit `lua5.1.dll`:
    /// set V2PROBE_LUA_DLL to its path (any Lua 5.1 build); without it the test does nothing.
    #[cfg(all(windows, target_arch = "x86"))]
    #[test]
    fn gm_functions_in_lua() {
        unsafe extern "system" {
            fn LoadLibraryA(name: *const u8) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }
        let Ok(dll) = std::env::var("V2PROBE_LUA_DLL") else { return };
        let dll = CString::new(dll).unwrap();
        let mut data = Box::new([0u8; 100]);
        data[..8].copy_from_slice(&(-5i64 * 32768).to_le_bytes());
        data[8..12].copy_from_slice(b"abc\0");
        let a = data.as_ptr() as usize;
        unsafe {
            let module = LoadLibraryA(dll.as_ptr().cast());
            assert!(!module.is_null(), "cannot load {dll:?}");
            let table = &raw mut crate::REAL;
            for (i, name) in NAMES.iter().enumerate() {
                (*table)[i] = GetProcAddress(module, name.as_ptr()) as usize;
            }
            let l = new_state();
            let run = |code: String| {
                let code = CString::new(code).unwrap();
                let status = real::<FnLoadFile>(IDX_luaL_loadstring)(l, code.as_ptr());
                assert_eq!(status, 0, "{}", value_text(l, -1));
                assert_eq!(real::<FnPCall>(IDX_lua_pcall)(l, 0, 1, 0), 0, "{}", value_text(l, -1));
                let out = value_text(l, -1);
                set_top(l, 0);
                out
            };
            let show = "local function s(...) local t = {} for i = 1, select('#', ...) do t[i] = tostring((select(i, ...))) end return table.concat(t, ' ') end ";
            let out = run(format!("{show} return s(gm.i64({a}), gm.u32({a}), gm.i8({a} + 7), gm.hex({a}, 3), gm.cstr({a} + 8), #gm.bytes({a}, 100), select(2, gm.heapscan()) > 100, gm.heapsize({a}), gm.heapsize({a} + 4), gm.heapblock({a} + 4) == {a}, gm.readable({a}, 100), gm.u32(16), gm.bytes(0, 4), gm.w32({a}, 7))"));
            assert_eq!(out, "-163840 4294803456 -1 0080fd abc 100 true 100 nil true true nil nil false");
            assert_eq!(data[0], 0, "a write without allow_write must not change memory");
            // With the FPU in single precision (as under Direct3D) Lua rounds large numbers;
            // the guard used around every script call must prevent that.
            let single: u16 = 0x007f;
            let mut saved: u16 = 0;
            core::arch::asm!("fnstcw word ptr [{p}]", p = in(reg) &mut saved, options(nostack));
            core::arch::asm!("fldcw word ptr [{p}]", p = in(reg) &single, options(nostack));
            let rounded = run("return tostring(822397680 + 4)".into());
            let exact = with_double_precision(|| run("return tostring(822397680 + 4)".into()));
            core::arch::asm!("fldcw word ptr [{p}]", p = in(reg) &saved, options(nostack));
            assert_eq!((rounded.as_str(), exact.as_str()), ("822397696", "822397684"));
            ALLOW_WRITE.store(true, Ordering::Relaxed);
            let out = run(format!("{show} return s(gm.w32({a}, 7), gm.u32({a}), gm.wbytes({a} + 8, 'xy'), gm.cstr({a} + 8), gm.w8(16, 1), type(gm.cfg), gm.dir() ~= '')"));
            ALLOW_WRITE.store(false, Ordering::Relaxed);
            assert_eq!(out, "true 7 true xyc false table true");
            let file = std::env::temp_dir().join(format!("gm-script-{}.bin", std::process::id()));
            let name = file.to_string_lossy().replace('\\', "/");
            let out = run(format!("{show} local h = gm.fopen('{name}') local w = gm.fwrite(h, 'ab', '\\0c') return s(w, gm.fclose(h), gm.fclose(h), #gm.readfile('{name}'), gm.fopen('?/nope'), gm.clock() >= 0)"));
            assert_eq!(out, "true true false 4 nil true");
            std::fs::remove_file(file).ok();
            // Optional: V2PROBE_SCRIPT names a script to compile and load (its top level only).
            if let Ok(script) = std::env::var("V2PROBE_SCRIPT") {
                let script = CString::new(script).unwrap();
                assert_eq!(real::<FnLoadFile>(IDX_luaL_loadfile)(l, script.as_ptr()), 0, "{}", value_text(l, -1));
                assert_eq!(real::<FnPCall>(IDX_lua_pcall)(l, 0, 0, 0), 0, "{}", value_text(l, -1));
                assert_eq!(run("return type(on_tick)".into()), "function");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn checks_real_memory() {
        let data = [1u8, 2, 3, 4];
        assert!(accessible(data.as_ptr() as usize, 4, false));
        assert!(!accessible(0, 4, false));
        assert!(!accessible(16, 4, false), "the null page is never mapped");
        assert!(!accessible(usize::MAX - 2, 8, false), "range wraps around");
    }
}
