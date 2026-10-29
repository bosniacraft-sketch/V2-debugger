// Write-watch: finds the code that changes a chosen memory location.
//
// Uses the processor's four debug registers on the calling thread. When watched memory is
// written (or read, if asked), the processor raises a single-step exception right after the
// instruction; a vectored exception handler records where that was and lets the game continue.
// Nothing in the exe is patched. The registers are per thread, so a watch only sees accesses
// made by the thread that set it: set watches from `before_tick`/`on_tick` (the game's
// simulation thread), not from `on_idle`.
//
// Script functions (added to the `gm` table by `script.cpp`):
//   gm.watch(addr, len [, reads])   len 1, 2 or 4 (addr aligned to len); returns slot 0-3 or nil.
//                                   With `reads` true, reads are reported as well as writes.
//   gm.unwatch([slot])              clears one slot, or all without an argument
//   gm.watch_hits()                 array of {slot, rva, count, callers}; `rva` is the address
//                                   after the accessing instruction, relative to the exe, and
//                                   `callers` a space-separated list of return addresses (rva)
//                                   found on the stack at the first hit. Clears the list.
//
// The debug registers are changed by raising a private exception and editing the thread
// context inside the handler, which is the supported way for a thread to change its own.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <algorithm>
#include <atomic>
#include <cstring>
#include <map>
#include <tuple>
#include <utility>

#include "proxy.h"

namespace watch {
namespace {

// Return addresses recorded per hit location.
const size_t CALLERS = 8;
// Stack words searched for return addresses.
const size_t STACK_WORDS = 256;

// A small lock over an SRWLOCK: the mingw win32 thread model has no std::mutex, and the
// exception handler needs a try-lock that never blocks.
struct Lock {
    SRWLOCK l = SRWLOCK_INIT;
    void lock() { AcquireSRWLockExclusive(&l); }
    bool try_lock() { return TryAcquireSRWLockExclusive(&l) != 0; }
    void unlock() { ReleaseSRWLockExclusive(&l); }
};
struct Guard {
    Lock& m;
    explicit Guard(Lock& m) : m(m) { m.lock(); }
    ~Guard() { m.unlock(); }
    Guard(const Guard&) = delete;
    Guard& operator=(const Guard&) = delete;
};

struct Callers {
    uintptr_t v[CALLERS] = {};
};
// Hits by (slot, address after the instruction): count and callers of the first hit.
using Hits = std::map<std::pair<int, uintptr_t>, std::pair<uint64_t, Callers>>;

Lock slots_lock;
Slot SLOTS[4];
Lock hits_lock;
Hits HITS;
std::atomic<bool> HANDLER_INSTALLED{false};
// Code range of the exe, for telling return addresses from other stack values.
std::atomic<uintptr_t> CODE_RANGE[2];

#if defined(_M_IX86) || defined(__i386__)

// Private exception code used to ask the handler to load the debug registers.
const DWORD APPLY_CODE = 0xE0474D57;

// The slots the handler loads into the debug registers on the next APPLY_CODE.
Lock pending_lock;
Slot PENDING[4];

// Values on the stack that point into the exe's code: the chain of callers, best effort
// (it can include stale values left by earlier calls).
Callers callers(uintptr_t esp) {
    Callers out;
    uintptr_t lo = CODE_RANGE[0].load(std::memory_order_relaxed);
    uintptr_t hi = CODE_RANGE[1].load(std::memory_order_relaxed);
    if (hi == 0 || IsBadReadPtr(reinterpret_cast<const void*>(esp), STACK_WORDS * 4)) {
        return out;
    }
    size_t n = 0;
    for (size_t i = 0; i < STACK_WORDS; ++i) {
        uint32_t w;
        std::memcpy(&w, reinterpret_cast<const void*>(esp + i * 4), 4);
        uintptr_t v = w;
        if (v >= lo && v < hi) {
            out.v[n++] = v;
            if (n == CALLERS) break;
        }
    }
    return out;
}

LONG CALLBACK handler(PEXCEPTION_POINTERS info) {
    DWORD code = info->ExceptionRecord->ExceptionCode;
    CONTEXT* ctx = info->ContextRecord;
    if (code == APPLY_CODE) {
        Slot slots[4];
        if (pending_lock.try_lock()) {
            std::copy(PENDING, PENDING + 4, slots);
            pending_lock.unlock();
        }
        DWORD* dr[4] = {&ctx->Dr0, &ctx->Dr1, &ctx->Dr2, &ctx->Dr3};
        for (int i = 0; i < 4; ++i) {
            *dr[i] = slots[i].len == 0 ? 0 : static_cast<DWORD>(slots[i].addr);
        }
        ctx->Dr6 = 0;
        ctx->Dr7 = dr7(slots);
        ctx->ContextFlags |= CONTEXT_DEBUG_REGISTERS;
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    if (code != EXCEPTION_SINGLE_STEP) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    DWORD hit = ctx->Dr6 & 0xf;
    if (hit == 0) {
        return EXCEPTION_CONTINUE_SEARCH; // a single step that is not ours
    }
    uintptr_t eip = ctx->Eip;
    uintptr_t esp = ctx->Esp;
    if (hits_lock.try_lock()) {
        for (int slot = 0; slot < 4; ++slot) {
            if ((hit & (1u << slot)) == 0) continue;
            auto key = std::make_pair(slot, eip);
            auto it = HITS.find(key);
            if (it == HITS.end()) {
                it = HITS.emplace(key, std::make_pair(uint64_t(0), callers(esp))).first;
            }
            it->second.first += 1;
        }
        hits_lock.unlock();
    }
    ctx->Dr6 = 0;
    ctx->ContextFlags |= CONTEXT_DEBUG_REGISTERS;
    return EXCEPTION_CONTINUE_EXECUTION;
}

// Loads the current slots into the calling thread's debug registers.
void apply(const Slot (&slots)[4]) {
    if (!HANDLER_INSTALLED.exchange(true)) {
        AddVectoredExceptionHandler(1, handler);
        // Code range: the exe image from its PE header (SizeOfImage).
        uintptr_t base = reinterpret_cast<uintptr_t>(GetModuleHandleA(nullptr));
        uint32_t lfanew, size;
        std::memcpy(&lfanew, reinterpret_cast<const void*>(base + 0x3c), 4);
        uintptr_t pe = base + lfanew;
        std::memcpy(&size, reinterpret_cast<const void*>(pe + 0x50), 4);
        CODE_RANGE[0].store(base, std::memory_order_relaxed);
        CODE_RANGE[1].store(base + size, std::memory_order_relaxed);
    }
    {
        Guard g(pending_lock);
        std::copy(slots, slots + 4, PENDING);
    }
    RaiseException(APPLY_CODE, 0, 0, nullptr);
}

#else

void apply(const Slot (&)[4]) {
    (void)HANDLER_INSTALLED;
    (void)CODE_RANGE;
    (void)STACK_WORDS;
}

#endif

} // namespace

// `gm.watch`: returns the slot, or -1.
int watch(uintptr_t addr, size_t len, bool reads) {
    Slot all[4];
    int slot;
    {
        Guard g(slots_lock);
        slot = add(SLOTS, addr, len, reads);
        if (slot < 0) return -1;
        std::copy(SLOTS, SLOTS + 4, all);
    }
    apply(all);
    return slot;
}

// `gm.unwatch`: clears one slot, or all (-1).
void unwatch(int slot) {
    Slot all[4];
    {
        Guard g(slots_lock);
        if (slot >= 0 && slot < 4) {
            SLOTS[slot] = Slot();
        } else if (slot < 0) {
            for (Slot& s : SLOTS) s = Slot();
        }
        std::copy(SLOTS, SLOTS + 4, all);
    }
    apply(all);
}

// `gm.watch_hits`: all hit locations so far, most frequent first; clears the list.
std::vector<Hit> take_hits() {
    Hits taken;
    {
        Guard g(hits_lock);
        taken.swap(HITS);
    }
    std::vector<Hit> out;
    out.reserve(taken.size());
    for (auto& kv : taken) {
        Hit h;
        h.slot = kv.first.first;
        h.at = kv.first.second;
        h.count = kv.second.first;
        for (uintptr_t c : kv.second.second.v) {
            if (c != 0) h.callers.push_back(c);
        }
        out.push_back(std::move(h));
    }
    std::sort(out.begin(), out.end(), [](const Hit& a, const Hit& b) {
        if (a.count != b.count) return a.count > b.count;
        return std::tie(a.slot, a.at, a.count, a.callers) < std::tie(b.slot, b.at, b.count, b.callers);
    });
    return out;
}

} // namespace watch
