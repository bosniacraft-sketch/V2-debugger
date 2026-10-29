//! Optional in-memory tick hook into v2game.exe, configured at runtime.
//!
//! The proxy ships with no addresses. You find them for your own game build and write them into
//! `v2probe.txt` next to this DLL. Without that file no hook is installed and the proxy only
//! logs Lua use. The hook checks that the patched location holds exactly the expected value
//! first, so a different game build is left untouched. Nothing is written to the exe on disk.
//!
//! Config format: one `key = value` per line, values in hex (0x...) or decimal, `#` comments.
//! Every key is also visible to scripts as `gm.cfg.<key>`, so you can add your own.
//!
//!   tick_slot       RVA of a vtable slot holding a method the game calls once per game day
//!   tick_expect     RVA of the function that slot holds now (sanity check before patching)
//!   gamestate_ptr   RVA of a global pointer your scripts want as `gm.gs()`       (optional)
//!   date_offset     offset of a 32-bit value inside that object, passed to the
//!                   script as `raw_date` and returned by `gm.date()`               (optional)
//!   allow_write     1 lets scripts write to game memory (every write is logged)  (optional)
//!
//! With the hook in place, each tick runs the script's `before_tick` and `on_tick`, see `script.rs`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::log;

unsafe extern "system" {
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    fn VirtualProtect(addr: *mut c_void, size: usize, new_protect: u32, old: *mut u32) -> i32;
}

const PAGE_READWRITE: u32 = 0x04;

pub fn read_config(path: &std::path::Path) -> Option<HashMap<String, usize>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        let parsed = match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
            Some(hex) => usize::from_str_radix(hex, 16).ok(),
            None => v.parse().ok(),
        };
        if let Some(n) = parsed {
            map.insert(k.trim().to_string(), n);
        }
    }
    Some(map)
}

static EXE_BASE: AtomicUsize = AtomicUsize::new(0);
static ORIG_TICK: AtomicUsize = AtomicUsize::new(0);
static GAMESTATE_PTR: AtomicUsize = AtomicUsize::new(0);
/// Offset of the raw date inside the game state, plus one (0 = not configured).
static DATE_OFF: AtomicUsize = AtomicUsize::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

unsafe fn read_u32(addr: usize) -> u32 {
    unsafe { std::ptr::read_unaligned(addr as *const u32) }
}

// The game's methods use MSVC `thiscall` (this in ECX), which only exists on 32-bit x86.
// Other targets get `C` so the crate still type-checks and unit-tests on a 64-bit host.
#[cfg(target_arch = "x86")]
type TickFn = unsafe extern "thiscall" fn(*mut c_void);
#[cfg(not(target_arch = "x86"))]
type TickFn = unsafe extern "C" fn(*mut c_void);

#[cfg(target_arch = "x86")]
unsafe extern "thiscall" fn tick_hook(this: *mut c_void) {
    unsafe { on_tick(this) }
}
#[cfg(not(target_arch = "x86"))]
unsafe extern "C" fn tick_hook(this: *mut c_void) {
    unsafe { on_tick(this) }
}

unsafe fn on_tick(this: *mut c_void) {
    let orig: TickFn = unsafe { std::mem::transmute(ORIG_TICK.load(Ordering::Relaxed)) };
    unsafe { crate::script::call(c"before_tick", TICKS.load(Ordering::Relaxed) + 1, raw_date()) };
    unsafe { orig(this) };
    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    let date = raw_date();
    match date {
        Some(d) => log(&format!("tick #{n}: raw date {d}")),
        None => log(&format!("tick #{n}")),
    }
    unsafe { crate::script::call(c"on_tick", n, date) };
    crate::script::quit_if_requested();
}

/// Load address of the exe (0 before the hooks are installed).
pub(crate) fn exe_base() -> usize {
    EXE_BASE.load(Ordering::Relaxed)
}

/// The object behind `gamestate_ptr`, or 0 when it is not configured or not set yet.
pub(crate) fn game_state() -> usize {
    let gs_ptr = GAMESTATE_PTR.load(Ordering::Relaxed);
    if gs_ptr != 0 { (unsafe { read_u32(gs_ptr) }) as usize } else { 0 }
}

/// The 32-bit value at `date_offset` in the game state, or None when either is missing.
pub(crate) fn raw_date() -> Option<i32> {
    let off = DATE_OFF.load(Ordering::Relaxed);
    let gs = game_state();
    if off == 0 || gs == 0 {
        return None;
    }
    Some(unsafe { read_u32(gs + off - 1) } as i32)
}

/// Daily ticks seen since the DLL was loaded.
pub(crate) fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Installs the configured hook. Called once from DllMain.
pub unsafe fn install(config: &HashMap<String, usize>) -> Result<String, String> {
    let base = unsafe { GetModuleHandleA(std::ptr::null()) } as usize;
    if base == 0 {
        return Err("no exe module".into());
    }
    EXE_BASE.store(base, Ordering::Relaxed);
    let get = |k: &str| config.get(k).copied().ok_or(format!("config key '{k}' missing"));
    let slot = base + get("tick_slot")?;
    let expect = base + get("tick_expect")?;
    if let Some(p) = config.get("gamestate_ptr") {
        GAMESTATE_PTR.store(base + p, Ordering::Relaxed);
    }
    if let Some(d) = config.get("date_offset") {
        DATE_OFF.store(d + 1, Ordering::Relaxed);
    }

    let current = unsafe { read_u32(slot) } as usize;
    if current != expect {
        return Err(format!("tick slot holds {current:#x}, expected {expect:#x}: different build, not patched"));
    }
    ORIG_TICK.store(current, Ordering::Relaxed);
    let mut old = 0u32;
    unsafe {
        if VirtualProtect(slot as *mut c_void, 4, PAGE_READWRITE, &mut old) == 0 {
            return Err("VirtualProtect failed".into());
        }
        std::ptr::write_unaligned(slot as *mut u32, tick_hook as *const () as usize as u32);
        VirtualProtect(slot as *mut c_void, 4, old, &mut old);
    }
    let script = crate::script::configure(config);
    crate::script::start_idle_thread();
    Ok(format!("exe base {base:#x}; daily tick hooked (slot {slot:#x}){script}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config() {
        let dir = std::env::temp_dir().join(format!("v2probe-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.txt");
        std::fs::write(&p, "# comment\ntick_slot = 0x1A2b\ndate_offset=40 # trailing\nbad line\n").unwrap();
        let c = read_config(&p).unwrap();
        assert_eq!((c["tick_slot"], c["date_offset"], c.len()), (0x1a2b, 40, 2));
        std::fs::remove_dir_all(dir).ok();
    }
}
