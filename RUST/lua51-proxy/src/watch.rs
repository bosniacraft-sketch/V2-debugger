//! Write-watch: finds the code that changes a chosen memory location.
//!
//! Uses the processor's four debug registers on the calling thread. When watched memory is
//! written (or read, if asked), the processor raises a single-step exception right after the
//! instruction; a vectored exception handler records where that was and lets the game continue.
//! Nothing in the exe is patched. The registers are per thread, so a watch only sees accesses
//! made by the thread that set it: set watches from `before_tick`/`on_tick` (the game's
//! simulation thread), not from `on_idle`.
//!
//! Script functions (added to the `gm` table by `script.rs`):
//!   gm.watch(addr, len [, reads])   len 1, 2 or 4 (addr aligned to len); returns slot 0-3 or nil.
//!                                   With `reads` true, reads are reported as well as writes.
//!   gm.unwatch([slot])              clears one slot, or all without an argument
//!   gm.watch_hits()                 array of {slot, rva, count, callers}; `rva` is the address
//!                                   after the accessing instruction, relative to the exe, and
//!                                   `callers` a space-separated list of return addresses (rva)
//!                                   found on the stack at the first hit. Clears the list.
//!
//! The debug registers are changed by raising a private exception and editing the thread
//! context inside the handler, which is the supported way for a thread to change its own.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Private exception code used to ask the handler to load the debug registers.
const APPLY_CODE: u32 = 0xE0474D57;
const EXCEPTION_SINGLE_STEP: u32 = 0x8000_0004;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const CONTEXT_DEBUG_REGISTERS: u32 = 0x0001_0010;
/// Return addresses recorded per hit location.
const CALLERS: usize = 8;
/// Stack words searched for return addresses.
const STACK_WORDS: usize = 256;

/// One watch slot: address, length (1, 2, 4) and whether reads count too. Length 0 = free.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Slot {
    pub addr: usize,
    pub len: usize,
    pub reads: bool,
}

static SLOTS: Mutex<[Slot; 4]> = Mutex::new([Slot { addr: 0, len: 0, reads: false }; 4]);
/// Hits by (slot, address after the instruction): count and callers of the first hit.
type Hits = BTreeMap<(usize, usize), (u64, [usize; CALLERS])>;
static HITS: Mutex<Hits> = Mutex::new(BTreeMap::new());
static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);
/// Code range of the exe, for telling return addresses from other stack values.
static CODE_RANGE: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

/// The DR7 control value for a set of slots: local-enable bit, access type and length per slot.
pub fn dr7(slots: &[Slot; 4]) -> u32 {
    let mut v = 0u32;
    for (i, s) in slots.iter().enumerate() {
        if s.len == 0 {
            continue;
        }
        let access = if s.reads { 0b11 } else { 0b01 };
        let len = match s.len {
            1 => 0b00,
            2 => 0b01,
            _ => 0b11,
        };
        v |= 1 << (i * 2);
        v |= access << (16 + i * 4);
        v |= len << (18 + i * 4);
    }
    v
}

/// Puts a watch in the first free slot. None when the request is invalid or all four are used.
pub fn add(slots: &mut [Slot; 4], addr: usize, len: usize, reads: bool) -> Option<usize> {
    if !matches!(len, 1 | 2 | 4) || addr == 0 || !addr.is_multiple_of(len) {
        return None;
    }
    let i = slots.iter().position(|s| s.len == 0)?;
    slots[i] = Slot { addr, len, reads };
    Some(i)
}

#[cfg(all(windows, target_arch = "x86"))]
mod imp {
    use super::*;

    #[repr(C)]
    struct ExceptionPointers {
        record: *mut u32,   // EXCEPTION_RECORD; ExceptionCode is its first field
        context: *mut u32,  // CONTEXT (x86), accessed by word index below
    }

    // Word indexes into the x86 CONTEXT structure.
    const CTX_FLAGS: usize = 0;
    const CTX_DR0: usize = 1;
    const CTX_DR6: usize = 5;
    const CTX_DR7: usize = 6;
    const CTX_EIP: usize = 184 / 4;
    const CTX_ESP: usize = 196 / 4;

    unsafe extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: *const c_void) -> *mut c_void;
        fn RaiseException(code: u32, flags: u32, count: u32, args: *const usize);
        fn GetModuleHandleA(name: *const u8) -> *mut c_void;
        fn IsBadReadPtr(p: *const c_void, n: usize) -> i32;
    }

    unsafe extern "system" fn handler(info: *mut ExceptionPointers) -> i32 {
        unsafe {
            let code = *(*info).record;
            let ctx = (*info).context;
            if code == APPLY_CODE {
                let slots = PENDING.lock().map(|g| *g).unwrap_or_default();
                for (i, s) in slots.iter().enumerate() {
                    *ctx.add(CTX_DR0 + i) = if s.len == 0 { 0 } else { s.addr as u32 };
                }
                *ctx.add(CTX_DR6) = 0;
                *ctx.add(CTX_DR7) = dr7(&slots);
                *ctx.add(CTX_FLAGS) |= CONTEXT_DEBUG_REGISTERS;
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            if code != EXCEPTION_SINGLE_STEP {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            let hit = *ctx.add(CTX_DR6) & 0xf;
            if hit == 0 {
                return EXCEPTION_CONTINUE_SEARCH; // a single step that is not ours
            }
            let eip = *ctx.add(CTX_EIP) as usize;
            let esp = *ctx.add(CTX_ESP) as usize;
            if let Ok(mut hits) = HITS.try_lock() {
                for slot in 0..4 {
                    if hit & (1 << slot) == 0 {
                        continue;
                    }
                    let entry = hits.entry((slot, eip)).or_insert_with(|| (0, callers(esp)));
                    entry.0 += 1;
                }
            }
            *ctx.add(CTX_DR6) = 0;
            *ctx.add(CTX_FLAGS) |= CONTEXT_DEBUG_REGISTERS;
            EXCEPTION_CONTINUE_EXECUTION
        }
    }

    /// Values on the stack that point into the exe's code: the chain of callers, best effort
    /// (it can include stale values left by earlier calls).
    unsafe fn callers(esp: usize) -> [usize; CALLERS] {
        let mut out = [0usize; CALLERS];
        let (lo, hi) = (CODE_RANGE[0].load(Ordering::Relaxed), CODE_RANGE[1].load(Ordering::Relaxed));
        let mut n = 0;
        unsafe {
            if hi == 0 || IsBadReadPtr(esp as *const c_void, STACK_WORDS * 4) != 0 {
                return out;
            }
            for i in 0..STACK_WORDS {
                let v = std::ptr::read_unaligned((esp + i * 4) as *const u32) as usize;
                if v >= lo && v < hi {
                    out[n] = v;
                    n += 1;
                    if n == CALLERS {
                        break;
                    }
                }
            }
        }
        out
    }

    /// The slots the handler loads into the debug registers on the next APPLY_CODE.
    static PENDING: Mutex<[Slot; 4]> = Mutex::new([Slot { addr: 0, len: 0, reads: false }; 4]);

    /// Loads the current slots into the calling thread's debug registers.
    pub unsafe fn apply(slots: [Slot; 4]) {
        unsafe {
            if !HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
                AddVectoredExceptionHandler(1, handler as *const c_void);
                // Code range: the exe image from its PE header (SizeOfImage).
                let base = GetModuleHandleA(std::ptr::null()) as usize;
                let pe = base + std::ptr::read_unaligned((base + 0x3c) as *const u32) as usize;
                let size = std::ptr::read_unaligned((pe + 0x50) as *const u32) as usize;
                CODE_RANGE[0].store(base, Ordering::Relaxed);
                CODE_RANGE[1].store(base + size, Ordering::Relaxed);
            }
            if let Ok(mut p) = PENDING.lock() {
                *p = slots;
            }
            RaiseException(APPLY_CODE, 0, 0, std::ptr::null());
        }
    }
}

#[cfg(not(all(windows, target_arch = "x86")))]
mod imp {
    use super::*;
    pub unsafe fn apply(_slots: [Slot; 4]) {
        let _ = (&HANDLER_INSTALLED, &CODE_RANGE, CONTEXT_DEBUG_REGISTERS, APPLY_CODE, EXCEPTION_SINGLE_STEP);
        let _ = (EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, STACK_WORDS, std::ptr::null::<c_void>());
    }
}

/// `gm.watch`: returns the slot, or None.
pub unsafe fn watch(addr: usize, len: usize, reads: bool) -> Option<usize> {
    let (slot, all) = {
        let mut slots = SLOTS.lock().ok()?;
        (add(&mut slots, addr, len, reads)?, *slots)
    };
    unsafe { imp::apply(all) };
    Some(slot)
}

/// `gm.unwatch`: clears one slot, or all.
pub unsafe fn unwatch(slot: Option<usize>) {
    let all = {
        let Ok(mut slots) = SLOTS.lock() else { return };
        match slot {
            Some(i) if i < 4 => slots[i] = Slot::default(),
            Some(_) => {}
            None => *slots = [Slot::default(); 4],
        }
        *slots
    };
    unsafe { imp::apply(all) };
}

/// One recorded hit location: slot, address after the instruction, count, callers.
pub type Hit = (usize, usize, u64, Vec<usize>);

/// `gm.watch_hits`: all hit locations so far, most frequent first; clears the list.
pub fn take_hits() -> Vec<Hit> {
    let Ok(mut hits) = HITS.lock() else { return Vec::new() };
    let mut out: Vec<Hit> = std::mem::take(&mut *hits)
        .into_iter()
        .map(|((slot, at), (count, callers))| (slot, at, count, callers.into_iter().filter(|c| *c != 0).collect()))
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2).then(a.cmp(b)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_rules() {
        let mut s = [Slot::default(); 4];
        assert_eq!(add(&mut s, 0x1000, 4, false), Some(0));
        assert_eq!(add(&mut s, 0x1002, 2, true), Some(1));
        assert_eq!(add(&mut s, 0x1001, 2, false), None, "address must be aligned to the length");
        assert_eq!(add(&mut s, 0x1000, 3, false), None, "length must be 1, 2 or 4");
        assert_eq!(add(&mut s, 0x1003, 1, false), Some(2));
        assert_eq!(add(&mut s, 0x2000, 4, false), Some(3));
        assert_eq!(add(&mut s, 0x3000, 4, false), None, "only four slots");
        // slot 0: write, 4 bytes; slot 1: read/write, 2 bytes; slot 2: write, 1 byte; slot 3: write, 4 bytes
        assert_eq!(dr7(&s), 0b0101_0101 | (0b1101 << 16) | (0b0111 << 20) | (0b0001 << 24) | (0b1101 << 28));
        assert_eq!(dr7(&[Slot::default(); 4]), 0);
    }

    /// The real thing: watch a variable, write it, and expect the write to be reported.
    #[cfg(all(windows, target_arch = "x86"))]
    #[test]
    fn reports_a_write() {
        let mut cell = Box::new([0u32; 4]);
        let addr = cell.as_mut_ptr() as usize + 4;
        unsafe {
            let slot = watch(addr, 4, false).expect("a free slot");
            for i in 0..3u32 {
                std::ptr::write_volatile((addr) as *mut u32, i + 1);
            }
            let _ = std::ptr::read_volatile(addr as *const u32); // reads are not reported
            std::ptr::write_volatile((addr - 4) as *mut u32, 9); // nor are neighbours
            unwatch(Some(slot));
            std::ptr::write_volatile(addr as *mut u32, 7); // nor writes after unwatch
        }
        let hits = take_hits();
        let total: u64 = hits.iter().filter(|h| h.0 == 0).map(|h| h.2).sum();
        assert_eq!(total, 3, "{hits:?}");
        assert!(take_hits().is_empty());
        assert_eq!(cell[1], 7);
    }
}
