// Optional in-memory tick hook into v2game.exe, configured at runtime.
//
// The proxy ships with no addresses. You find them for your own game build and write them into
// `v2probe.txt` next to this DLL. Without that file no hook is installed and the proxy only logs
// Lua use. The hook checks that the patched location holds exactly the expected value first, so a
// different game build is left untouched. Nothing is written to the exe on disk.
//
// Config format: one `key = value` per line, values in hex (0x...) or decimal, `#` comments.
// Every key is also visible to scripts as `gm.cfg.<key>`, so you can add your own.
//
//   tick_slot       RVA of a vtable slot holding a method the game calls once per game day
//   tick_expect     RVA of the function that slot holds now (sanity check before patching)
//   gamestate_ptr   RVA of a global pointer your scripts want as `gm.gs()`       (optional)
//   date_offset     offset of a 32-bit value inside that object, passed to the
//                   script as `raw_date` and returned by `gm.date()`               (optional)
//   allow_write     1 lets scripts write to game memory (every write is logged)  (optional)
//
// With the hook in place, each tick runs the script's `before_tick` and `on_tick`, see script.cpp.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <atomic>
#include <cstdio>
#include <cstring>

#include "proxy.h"

namespace {

std::atomic<uintptr_t> exe_base_{0};
std::atomic<uintptr_t> orig_tick{0};
std::atomic<uintptr_t> gamestate_ptr{0};
// Offset of the raw date inside the game state, plus one (0 = not configured).
std::atomic<uintptr_t> date_off{0};
std::atomic<uint64_t> tick_count{0};

uint32_t read_u32(uintptr_t addr) {
    uint32_t v;
    std::memcpy(&v, reinterpret_cast<const void*>(addr), 4);
    return v;
}

std::string hex(uintptr_t v) {
    char buf[32];
    std::snprintf(buf, sizeof buf, "%#lx", static_cast<unsigned long>(v));
    return buf;
}

// The game's methods use MSVC `thiscall` (this in ECX). MSVC only allows that convention on member
// functions, so there the original is called, and the hook defined, as a member function; GCC and
// Clang accept `__thiscall` on plain functions. Other targets get the default convention so the
// file still compiles.
#if defined(_MSC_VER) && defined(_M_IX86)
struct TickObject {
    void tick();  // never defined: only the member pointer type is used
    void hook();
};
typedef void (TickObject::*TickMethod)();
static_assert(sizeof(TickMethod) == sizeof(void*), "single-inheritance member pointer");

void call_original(void* self) {
    uintptr_t addr = orig_tick.load();
    TickMethod m;
    std::memcpy(&m, &addr, sizeof m);
    (static_cast<TickObject*>(self)->*m)();
}
#else
#if defined(__i386__)
typedef void(__thiscall* TickFn)(void* self);
#else
typedef void (*TickFn)(void* self);
#endif
void call_original(void* self) { reinterpret_cast<TickFn>(orig_tick.load())(self); }
#endif

void on_tick(void* self) {
    int32_t date;
    bool has_date = hooks::raw_date(&date);
    script::call("before_tick", tick_count.load() + 1, has_date ? &date : nullptr);
    call_original(self);
    uint64_t n = ++tick_count;
    has_date = hooks::raw_date(&date);
    log_line("tick #" + std::to_string(n) + (has_date ? ": raw date " + std::to_string(date) : std::string()));
    script::call("on_tick", n, has_date ? &date : nullptr);
    script::quit_if_requested();
}

#if defined(_MSC_VER) && defined(_M_IX86)
void TickObject::hook() { on_tick(this); }
void* tick_hook_address() {
    TickMethod m = &TickObject::hook;
    void* p;
    std::memcpy(&p, &m, sizeof p);
    return p;
}
#else
#if defined(__i386__)
void __thiscall tick_hook(void* self) { on_tick(self); }
#else
void tick_hook(void* self) { on_tick(self); }
#endif
void* tick_hook_address() { return reinterpret_cast<void*>(&tick_hook); }
#endif

}  // namespace

namespace hooks {

bool read_config(const std::wstring& path, Config* out) {
    FILE* f = _wfopen(path.c_str(), L"rb");
    if (!f) return false;
    std::string text;
    char buf[4096];
    size_t n;
    while ((n = std::fread(buf, 1, sizeof buf, f)) > 0) text.append(buf, n);
    std::fclose(f);
    *out = parse_config(text);
    return true;
}

uintptr_t exe_base() { return exe_base_.load(); }

uintptr_t game_state() {
    uintptr_t p = gamestate_ptr.load();
    return p ? read_u32(p) : 0;
}

bool raw_date(int32_t* out) {
    uintptr_t off = date_off.load();
    uintptr_t gs = game_state();
    if (off == 0 || gs == 0) return false;
    *out = static_cast<int32_t>(read_u32(gs + off - 1));
    return true;
}

uint64_t ticks() { return tick_count.load(); }

std::string install(const Config& config) {
    uintptr_t base = reinterpret_cast<uintptr_t>(GetModuleHandleA(nullptr));
    if (base == 0) return "exe hooks NOT installed: no exe module";
    exe_base_ = base;
    auto slot_it = config.find("tick_slot");
    auto expect_it = config.find("tick_expect");
    if (slot_it == config.end()) return "exe hooks NOT installed: config key 'tick_slot' missing";
    if (expect_it == config.end()) return "exe hooks NOT installed: config key 'tick_expect' missing";
    uintptr_t slot = base + slot_it->second;
    uintptr_t expect = base + expect_it->second;
    auto gs = config.find("gamestate_ptr");
    if (gs != config.end()) gamestate_ptr = base + gs->second;
    auto d = config.find("date_offset");
    if (d != config.end()) date_off = d->second + 1;

    uintptr_t current = read_u32(slot);
    if (current != expect) {
        return "exe hooks NOT installed: tick slot holds " + hex(current) + ", expected " + hex(expect) +
               ": different build, not patched";
    }
    orig_tick = current;
    DWORD old = 0;
    if (!VirtualProtect(reinterpret_cast<void*>(slot), 4, PAGE_READWRITE, &old)) {
        return "exe hooks NOT installed: VirtualProtect failed";
    }
    uint32_t hook = static_cast<uint32_t>(reinterpret_cast<uintptr_t>(tick_hook_address()));
    std::memcpy(reinterpret_cast<void*>(slot), &hook, 4);
    VirtualProtect(reinterpret_cast<void*>(slot), 4, old, &old);
    std::string script = script::configure(config);
    script::start_idle_thread();
    return "exe base " + hex(base) + "; daily tick hooked (slot " + hex(slot) + ")" + script;
}

}  // namespace hooks
