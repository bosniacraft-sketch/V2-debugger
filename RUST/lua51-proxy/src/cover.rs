//! Coverage: which functions of the exe run during a chosen stretch of the game.
//!
//! The script loads a list of function entry addresses, then "arms" them: the first
//! byte of each is replaced, in memory only, by a breakpoint instruction. The first time a
//! function is entered the breakpoint fires once; a vectored exception handler puts the original
//! byte back, notes the order and the thread, and resumes the function. Every function therefore
//! costs one exception at most, and the game runs at normal speed afterwards.
//!
//! Script functions (added to the `gm` table by `script.rs`):
//!   gm.cover_load(path)     reads the list: one hex RVA per line, `#` comments. Returns the count.
//!   gm.cover_arm()          arms every listed function and forgets earlier hits. Returns the count.
//!   gm.cover_disarm()       restores every byte still armed.
//!   gm.cover_hits()         text, one line per function hit: `order rva thread_id`, in hit order.
//!
//! The list comes from a disassembler and must contain real function entries only: arming an
//! address that is data or the middle of an instruction corrupts the game. Use a spare copy of the game.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

/// One listed function: its address, its original first byte, and when armed/hit.
pub struct Entry {
    pub addr: usize,
    original: AtomicU32,
    /// 0 = not armed, 1 = armed, 2 = hit and restored.
    state: AtomicU32,
    order: AtomicU32,
    thread: AtomicU32,
}

/// The table, sorted by address. Leaked on purpose: the exception handler reads it without locks.
static TABLE: AtomicPtr<Vec<Entry>> = AtomicPtr::new(std::ptr::null_mut());
static NEXT_ORDER: AtomicU32 = AtomicU32::new(0);
static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

fn table() -> Option<&'static Vec<Entry>> {
    let p = TABLE.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

/// Parses the list text into sorted, de-duplicated RVAs.
pub fn parse_list(text: &str) -> Vec<usize> {
    let mut out: Vec<usize> = text
        .lines()
        .filter_map(|l| {
            let l = l.split('#').next().unwrap_or("").trim();
            let l = l.strip_prefix("0x").unwrap_or(l);
            usize::from_str_radix(l, 16).ok()
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Replaces the table with `rvas` placed at `base`. Must not be called while armed.
pub fn load(base: usize, rvas: &[usize]) -> usize {
    let entries: Vec<Entry> = rvas
        .iter()
        .map(|r| Entry { addr: base + r, original: AtomicU32::new(0), state: AtomicU32::new(0), order: AtomicU32::new(0), thread: AtomicU32::new(0) })
        .collect();
    let n = entries.len();
    // The previous table (if any) is leaked: a handler on another thread may still be reading it.
    TABLE.store(Box::into_raw(Box::new(entries)), Ordering::Release);
    n
}

fn find(addr: usize) -> Option<&'static Entry> {
    let t = table()?;
    t.binary_search_by(|e| e.addr.cmp(&addr)).ok().map(|i| &t[i])
}

/// Text of all hits, in hit order: `order rva thread_id`.
pub fn hits(base: usize) -> String {
    let Some(t) = table() else { return String::new() };
    let mut rows: Vec<(u32, usize, u32)> = t
        .iter()
        .filter(|e| e.state.load(Ordering::Relaxed) == 2)
        .map(|e| (e.order.load(Ordering::Relaxed), e.addr - base, e.thread.load(Ordering::Relaxed)))
        .collect();
    rows.sort_unstable();
    let mut s = String::with_capacity(rows.len() * 20);
    for (order, rva, thread) in rows {
        s.push_str(&format!("{order} {rva:x} {thread}\n"));
    }
    s
}

#[cfg(all(windows, target_arch = "x86"))]
mod imp {
    use super::*;
    use std::ffi::c_void;

    const EXCEPTION_BREAKPOINT: u32 = 0x8000_0003;
    const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    const CTX_EIP: usize = 184 / 4;
    const INT3: u8 = 0xcc;

    #[repr(C)]
    struct ExceptionPointers {
        record: *mut u32,  // EXCEPTION_RECORD: code at word 0, address at word 3
        context: *mut u32, // CONTEXT (x86)
    }

    unsafe extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: *const c_void) -> *mut c_void;
        fn VirtualProtect(addr: *mut c_void, size: usize, new_protect: u32, old: *mut u32) -> i32;
        fn FlushInstructionCache(process: *mut c_void, addr: *const c_void, size: usize) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
        fn GetCurrentThreadId() -> u32;
    }

    unsafe extern "system" fn handler(info: *mut ExceptionPointers) -> i32 {
        unsafe {
            if *(*info).record != EXCEPTION_BREAKPOINT {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            let addr = *(*info).record.add(3) as usize;
            let Some(e) = find(addr) else { return EXCEPTION_CONTINUE_SEARCH };
            // Armed -> hit: only the first thread to get here restores the byte and records it.
            // A second thread that trapped on the same byte just resumes at the function.
            if e.state.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                std::ptr::write_volatile(addr as *mut u8, e.original.load(Ordering::Relaxed) as u8);
                FlushInstructionCache(GetCurrentProcess(), addr as *const c_void, 1);
                e.order.store(NEXT_ORDER.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
                e.thread.store(GetCurrentThreadId(), Ordering::Relaxed);
            } else if e.state.load(Ordering::Acquire) == 0 {
                return EXCEPTION_CONTINUE_SEARCH; // a breakpoint of the game's own at this address
            }
            *(*info).context.add(CTX_EIP) = addr as u32;
            EXCEPTION_CONTINUE_EXECUTION
        }
    }

    /// Makes the pages holding the listed entries writable (they stay executable).
    unsafe fn unprotect(t: &[Entry]) -> bool {
        let (Some(first), Some(last)) = (t.first(), t.last()) else { return false };
        let start = first.addr & !0xfff;
        let len = (last.addr | 0xfff) + 1 - start;
        let mut old = 0u32;
        unsafe { VirtualProtect(start as *mut c_void, len, PAGE_EXECUTE_READWRITE, &mut old) != 0 }
    }

    pub unsafe fn arm() -> usize {
        let Some(t) = table() else { return 0 };
        unsafe {
            if !HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
                AddVectoredExceptionHandler(1, handler as *const c_void);
            }
            if !unprotect(t) {
                return 0;
            }
            NEXT_ORDER.store(0, Ordering::Relaxed);
            let mut armed = 0;
            for e in t {
                if e.state.load(Ordering::Acquire) != 1 {
                    let p = e.addr as *mut u8;
                    e.original.store(std::ptr::read_volatile(p) as u32, Ordering::Relaxed);
                    e.order.store(0, Ordering::Relaxed);
                    e.state.store(1, Ordering::Release);
                    std::ptr::write_volatile(p, INT3);
                }
                armed += 1;
            }
            FlushInstructionCache(GetCurrentProcess(), std::ptr::null(), 0);
            armed
        }
    }

    pub unsafe fn disarm() -> usize {
        let Some(t) = table() else { return 0 };
        let mut restored = 0;
        unsafe {
            for e in t {
                // Armed -> not armed. A thread already trapped on this byte finds state 0 only if
                // it lost the race; give it the restored byte first.
                if e.state.load(Ordering::Acquire) == 1 {
                    std::ptr::write_volatile(e.addr as *mut u8, e.original.load(Ordering::Relaxed) as u8);
                    if e.state.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        e.state.store(3, Ordering::Release); // restored without a hit
                        restored += 1;
                    }
                }
            }
            FlushInstructionCache(GetCurrentProcess(), std::ptr::null(), 0);
        }
        restored
    }
}

#[cfg(not(all(windows, target_arch = "x86")))]
mod imp {
    use super::*;
    pub unsafe fn arm() -> usize {
        let _ = (&HANDLER_INSTALLED, &NEXT_ORDER, find(0).map(|e| e.original.load(Ordering::Relaxed) + e.state.load(Ordering::Relaxed)));
        0
    }
    pub unsafe fn disarm() -> usize {
        0
    }
}

pub use imp::{arm, disarm};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lists() {
        assert_eq!(parse_list("# comment\n1f0\n0x20 # entry\n\n1f0\nzz\n10\n"), vec![0x10, 0x20, 0x1f0]);
        assert!(parse_list("").is_empty());
    }

    /// Arms two real functions of the test program, calls one, and expects exactly that hit.
    #[cfg(all(windows, target_arch = "x86"))]
    #[test]
    fn records_first_calls_only() {
        #[inline(never)]
        extern "C" fn called(x: u32) -> u32 {
            std::hint::black_box(x).wrapping_mul(3).wrapping_add(1)
        }
        #[inline(never)]
        extern "C" fn not_called(x: u32) -> u32 {
            std::hint::black_box(x).wrapping_mul(7).wrapping_add(5)
        }
        let (a, b) = (called as usize, not_called as usize);
        assert_ne!(a, b);
        let base = a.min(b) & !0xffff;
        assert_eq!(load(base, &parse_list(&format!("{:x}\n{:x}\n", a - base, b - base))), 2);
        unsafe {
            assert_eq!(arm(), 2);
            let f: extern "C" fn(u32) -> u32 = std::hint::black_box(called);
            assert_eq!(f(4), 13);
            assert_eq!(f(5), 16);
            assert_eq!(disarm(), 1, "only the function that never ran was still armed");
        }
        let text = hits(base);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{text}");
        assert!(lines[0].starts_with(&format!("1 {:x} ", a - base)), "{text}");
        let g: extern "C" fn(u32) -> u32 = std::hint::black_box(not_called);
        assert_eq!(g(1), 12, "a disarmed function runs normally");
    }
}
