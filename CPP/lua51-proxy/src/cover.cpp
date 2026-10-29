// Coverage: which functions of the exe run during a chosen stretch of the game.
//
// The script loads a list of function entry addresses, then "arms" them: the first
// byte of each is replaced, in memory only, by a breakpoint instruction. The first time a
// function is entered the breakpoint fires once; a vectored exception handler puts the original
// byte back, notes the order and the thread, and resumes the function. Every function therefore
// costs one exception at most, and the game runs at normal speed afterwards.
//
// Script functions (added to the `gm` table by `script.cpp`):
//   gm.cover_load(path)     reads the list: one hex RVA per line, `#` comments. Returns the count.
//   gm.cover_arm()          arms every listed function and forgets earlier hits. Returns the count.
//   gm.cover_disarm()       restores every byte still armed.
//   gm.cover_hits()         text, one line per function hit: `order rva thread_id`, in hit order.
//
// The list comes from a disassembler and must contain real function entries only: arming an
// address that is data or the middle of an instruction corrupts the game. Use a spare copy of the game.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <algorithm>
#include <atomic>
#include <cstdio>
#include <tuple>

#include "proxy.h"

namespace cover {
namespace {

// One listed function: its address, its original first byte, and when armed/hit.
struct Entry {
    uintptr_t addr = 0;
    std::atomic<uint32_t> original{0};
    // 0 = not armed, 1 = armed, 2 = hit and restored, 3 = restored without a hit.
    std::atomic<uint32_t> state{0};
    std::atomic<uint32_t> order{0};
    std::atomic<uint32_t> thread{0};
};
using Table = std::vector<Entry>;

// The table, sorted by address. Leaked on purpose: the exception handler reads it without locks.
std::atomic<Table*> TABLE{nullptr};
std::atomic<uint32_t> NEXT_ORDER{0};
std::atomic<bool> HANDLER_INSTALLED{false};

Table* table() { return TABLE.load(std::memory_order_acquire); }

Entry* find(uintptr_t addr) {
    Table* t = table();
    if (!t) return nullptr;
    auto it = std::lower_bound(t->begin(), t->end(), addr,
                               [](const Entry& e, uintptr_t a) { return e.addr < a; });
    if (it == t->end() || it->addr != addr) return nullptr;
    return &*it;
}

#if defined(_M_IX86) || defined(__i386__)

const BYTE INT3 = 0xcc;

LONG CALLBACK handler(PEXCEPTION_POINTERS info) {
    if (info->ExceptionRecord->ExceptionCode != EXCEPTION_BREAKPOINT) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    uintptr_t addr = reinterpret_cast<uintptr_t>(info->ExceptionRecord->ExceptionAddress);
    Entry* e = find(addr);
    if (!e) return EXCEPTION_CONTINUE_SEARCH;
    // Armed -> hit: only the first thread to get here restores the byte and records it.
    // A second thread that trapped on the same byte just resumes at the function.
    uint32_t expected = 1;
    if (e->state.compare_exchange_strong(expected, 2, std::memory_order_acq_rel, std::memory_order_acquire)) {
        *reinterpret_cast<volatile BYTE*>(addr) = static_cast<BYTE>(e->original.load(std::memory_order_relaxed));
        FlushInstructionCache(GetCurrentProcess(), reinterpret_cast<LPCVOID>(addr), 1);
        e->order.store(NEXT_ORDER.fetch_add(1, std::memory_order_relaxed) + 1, std::memory_order_relaxed);
        e->thread.store(GetCurrentThreadId(), std::memory_order_relaxed);
    } else if (e->state.load(std::memory_order_acquire) == 0) {
        return EXCEPTION_CONTINUE_SEARCH; // a breakpoint of the game's own at this address
    }
    info->ContextRecord->Eip = static_cast<DWORD>(addr);
    return EXCEPTION_CONTINUE_EXECUTION;
}

// Makes the pages holding the listed entries writable (they stay executable).
bool unprotect(const Table& t) {
    if (t.empty()) return false;
    uintptr_t start = t.front().addr & ~uintptr_t(0xfff);
    size_t len = (t.back().addr | 0xfff) + 1 - start;
    DWORD old = 0;
    return VirtualProtect(reinterpret_cast<LPVOID>(start), len, PAGE_EXECUTE_READWRITE, &old) != 0;
}

#endif

} // namespace

// Replaces the table with `rvas` placed at `base`. Must not be called while armed.
size_t load(uintptr_t base, const std::vector<uintptr_t>& rvas) {
    Table* entries = new Table(rvas.size());
    for (size_t i = 0; i < rvas.size(); ++i) (*entries)[i].addr = base + rvas[i];
    size_t n = entries->size();
    // The previous table (if any) is leaked: a handler on another thread may still be reading it.
    TABLE.store(entries, std::memory_order_release);
    return n;
}

// Text of all hits, in hit order: `order rva thread_id`.
std::string hits(uintptr_t base) {
    Table* t = table();
    if (!t) return std::string();
    std::vector<std::tuple<uint32_t, uintptr_t, uint32_t>> rows;
    for (const Entry& e : *t) {
        if (e.state.load(std::memory_order_relaxed) == 2) {
            rows.emplace_back(e.order.load(std::memory_order_relaxed), e.addr - base,
                              e.thread.load(std::memory_order_relaxed));
        }
    }
    std::sort(rows.begin(), rows.end());
    std::string s;
    s.reserve(rows.size() * 20);
    char buf[64];
    for (const auto& r : rows) {
        std::snprintf(buf, sizeof buf, "%lu %llx %lu\n", static_cast<unsigned long>(std::get<0>(r)),
                      static_cast<unsigned long long>(std::get<1>(r)), static_cast<unsigned long>(std::get<2>(r)));
        s += buf;
    }
    return s;
}

#if defined(_M_IX86) || defined(__i386__)

size_t arm() {
    Table* t = table();
    if (!t) return 0;
    if (!HANDLER_INSTALLED.exchange(true)) {
        AddVectoredExceptionHandler(1, handler);
    }
    if (!unprotect(*t)) return 0;
    NEXT_ORDER.store(0, std::memory_order_relaxed);
    size_t armed = 0;
    for (Entry& e : *t) {
        if (e.state.load(std::memory_order_acquire) != 1) {
            volatile BYTE* p = reinterpret_cast<volatile BYTE*>(e.addr);
            e.original.store(*p, std::memory_order_relaxed);
            e.order.store(0, std::memory_order_relaxed);
            e.state.store(1, std::memory_order_release);
            *p = INT3;
        }
        armed += 1;
    }
    FlushInstructionCache(GetCurrentProcess(), nullptr, 0);
    return armed;
}

size_t disarm() {
    Table* t = table();
    if (!t) return 0;
    size_t restored = 0;
    for (Entry& e : *t) {
        // Armed -> not armed. A thread already trapped on this byte finds state 0 only if
        // it lost the race; give it the restored byte first.
        if (e.state.load(std::memory_order_acquire) == 1) {
            *reinterpret_cast<volatile BYTE*>(e.addr) = static_cast<BYTE>(e.original.load(std::memory_order_relaxed));
            uint32_t expected = 1;
            if (e.state.compare_exchange_strong(expected, 2, std::memory_order_acq_rel, std::memory_order_acquire)) {
                e.state.store(3, std::memory_order_release); // restored without a hit
                restored += 1;
            }
        }
    }
    FlushInstructionCache(GetCurrentProcess(), nullptr, 0);
    return restored;
}

#else

size_t arm() {
    (void)HANDLER_INSTALLED;
    (void)NEXT_ORDER;
    (void)find(0);
    return 0;
}

size_t disarm() { return 0; }

#endif

} // namespace cover
