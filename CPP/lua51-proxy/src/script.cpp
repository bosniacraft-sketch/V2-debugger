// Scripting: a Lua script you write drives the measurements, so a new experiment is a script
// edit instead of a rebuild of this DLL.
//
// On the first daily tick the proxy creates its own Lua state in the real `lua5.1.dll` (separate
// from the game's, with the full standard libraries) and loads `v2probe_script.lua` from the
// folder of this DLL. The file is reloaded whenever it changes on disk, also while the game runs.
// Each day the proxy calls these optional global functions of the script (`raw_date` is the
// value at `date_offset`, or nil when that key is not configured):
//
//   before_tick(n, raw_date)           before the game's own daily tick runs (date = old day)
//   on_tick(n, raw_date)               after it
//   on_idle(n, raw_date)               about ten times a second from a background thread, also
//                                      while the game is paused (n = ticks so far). The game may
//                                      be mid-update: read with care.
//
// The script sees one global table, `gm`. Every address is a plain number; every read is
// range-checked against the process's committed memory and returns nil when it is not readable.
//
//   gm.base()  gm.gs()                 exe load address; game state pointer (0 when none)
//   gm.cfg                             table of all numeric keys of v2probe.txt
//   gm.u8 u16 u32 i8 i16 i32 i64 f32 f64 (addr)   one value (i64 as a Lua number, exact to 2^53)
//   gm.ptr(addr)                       same as u32
//   gm.bytes(addr, n)                  n raw bytes as a string, for io.write into a binary dump
//   gm.hex(addr, n)                    the same as hex text
//   gm.cstr(addr [, max])              zero-terminated text (max 256 by default)
//   gm.str(addr)                       an MSVC std::string object at addr
//   gm.readable(addr, n)               true when the whole range can be read
//   gm.heapscan()                      lists every heap block of the process; returns count, bytes
//   gm.heapsize(addr)                  size of the block that starts at addr (as of the last scan), or nil
//   gm.heapblock(addr)                 start, size of the block that contains addr, or nil
//   gm.tick()  gm.date()               tick number; raw date (nil when not configured)
//   gm.dir()                           folder of this DLL, with a trailing backslash
//   gm.log(...)                        writes to v2probe_proxy.log
//   gm.clock()                         seconds since the DLL was loaded
//   gm.fopen(path [, append])          opens a file for binary writing; returns a handle, or nil + error
//   gm.fwrite(handle, ...)             writes its string arguments; gm.fclose(handle)
//   gm.readfile(path)                  a whole file as a string, or nil
//                                      (the game's Lua build has no `io` or `os` library)
//   gm.quit()                          ends the game process once this tick's work is done
//
// Writes exist only when v2probe.txt has `allow_write = 1`, and each one is logged:
//
//   gm.w8 w16 w32 w64 (addr, value)    returns true when written
//   gm.wbytes(addr, string)
//
// Write-watch (see `watch.cpp`; call from before_tick/on_tick, it watches the calling thread):
//
//   gm.watch(addr, len [, reads])      reports code that writes (or also reads) addr; slot 0-3 or nil
//   gm.unwatch([slot])                 clears one slot, or all
//   gm.watch_hits()                    array of {slot=, rva=, count=, callers="rva rva ..."}; clears it
//
// Coverage (see `cover.cpp`): which listed functions run, and in what order:
//
//   gm.cover_load(path)                reads a list of function RVAs (hex, one per line); returns the count
//   gm.cover_arm()  gm.cover_disarm()  start and stop recording first calls
//   gm.cover_hits()                    text: one `order rva thread_id` line per function hit
//
// Paths given to the script functions are UTF-8. This file holds no addresses and no game data.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <errno.h>
#include <float.h>
#include <stdio.h>
#include <string.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <climits>
#include <cmath>
#include <mutex>
#include <thread>

#include "proxy.h"

namespace {

const wchar_t SCRIPT_FILE_W[] = L"v2probe_script.lua";
const char SCRIPT_FILE[] = "v2probe_script.lua";
// Largest single read or write a script may ask for.
const size_t MAX_RANGE = size_t(64) << 20;

// ---------------------------------------------------------------- memory checks

// True when every page of addr..addr+len is committed and readable (and writable if asked).
bool accessible(uintptr_t addr, size_t len, bool write) {
    if (addr == 0 || len == 0 || len > MAX_RANGE) return false;
    uintptr_t end = addr + len;
    if (end < addr) return false; // wraps around
    uintptr_t at = addr;
    while (at < end) {
        MEMORY_BASIC_INFORMATION info;
        memset(&info, 0, sizeof info);
        if (VirtualQuery(reinterpret_cast<const void*>(at), &info, sizeof info) == 0) return false;
        if (!region_ok(info.State, info.Protect, write)) return false;
        uintptr_t next = reinterpret_cast<uintptr_t>(info.BaseAddress) + info.RegionSize;
        if (next <= at) return false;
        at = next;
    }
    return true;
}

// ---------------------------------------------------------------- state

std::atomic<lua_State*> g_state{nullptr};
std::atomic<bool> g_allow_write{false};
std::atomic<bool> g_quit{false};
std::mutex g_config_lock;
Config g_config;

// Modification time of the script when last loaded.
struct Loaded {
    bool checked = false; // false = never looked yet
    bool exists = false;  // false = no file then
    FILETIME time = {0, 0};
};
std::mutex g_loaded_lock;
Loaded g_loaded;

// Raw date of the tick being handled.
std::atomic<bool> g_has_date{false};
std::atomic<int32_t> g_date{0};

std::mutex g_call_lock;

const auto g_start = std::chrono::steady_clock::now();

std::wstring utf8_to_wide(const char* s, size_t n) {
    if (n == 0) return std::wstring();
    int len = MultiByteToWideChar(CP_UTF8, 0, s, static_cast<int>(n), nullptr, 0);
    if (len <= 0) return std::wstring();
    std::wstring w(static_cast<size_t>(len), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s, static_cast<int>(n), &w[0], len);
    return w;
}

// The whole file, or false when it cannot be read.
bool read_whole(const std::wstring& path, std::string* out) {
    FILE* f = nullptr;
    if (path.empty() || _wfopen_s(&f, path.c_str(), L"rb") != 0 || !f) return false;
    out->clear();
    char buf[65536];
    size_t got;
    while ((got = fread(buf, 1, sizeof buf, f)) > 0) out->append(buf, got);
    bool ok = ferror(f) == 0;
    fclose(f);
    return ok;
}

// "0x" and lowercase hex, like Rust's {:#x}.
std::string hex_addr(uintptr_t v, bool prefix) {
    char buf[32];
    char* p = buf + sizeof buf;
    *--p = '\0';
    do {
        *--p = "0123456789abcdef"[v & 15];
        v >>= 4;
    } while (v);
    return (prefix ? "0x" : "") + std::string(p);
}

// ---------------------------------------------------------------- Lua plumbing

// Argument i as an address or size; 0 when missing or negative.
uintptr_t arg(lua_State* l, int i) {
    double v = lua.tonumber(l, i);
    if (!std::isfinite(v) || v <= 0.0) return 0;
    if (v >= 18446744073709551616.0) return static_cast<uintptr_t>(UINT64_MAX);
    return static_cast<uintptr_t>(static_cast<uint64_t>(v));
}

// Argument i as bytes (strings and numbers); empty for anything else.
std::pair<const uint8_t*, size_t> arg_bytes(lua_State* l, int i) {
    size_t len = 0;
    const char* p = lua.tolstring(l, i, &len);
    if (!p) return {nullptr, 0};
    return {reinterpret_cast<const uint8_t*>(p), len};
}

void push_bytes(lua_State* l, const void* p, size_t n) {
    lua.pushlstring(l, static_cast<const char*>(p), n);
}

// The bytes at addr, or null when any of them is not readable.
const uint8_t* mem(uintptr_t addr, size_t len) {
    return accessible(addr, len, false) ? reinterpret_cast<const uint8_t*>(addr) : nullptr;
}

// A Lua number as a 64-bit integer, saturating (NaN = 0).
int64_t to_i64(double v) {
    if (std::isnan(v)) return 0;
    if (v >= 9223372036854775807.0) return INT64_MAX;
    if (v <= -9223372036854775808.0) return INT64_MIN;
    return static_cast<int64_t>(v);
}

template <typename T>
int read_value(lua_State* l) {
    const uint8_t* b = mem(arg(l, 1), sizeof(T));
    if (b) {
        T v;
        memcpy(&v, b, sizeof v);
        lua.pushnumber(l, static_cast<double>(v));
    } else {
        lua.pushnil(l);
    }
    return 1;
}

static int gm_u8(lua_State* l) { return read_value<uint8_t>(l); }
static int gm_u16(lua_State* l) { return read_value<uint16_t>(l); }
static int gm_u32(lua_State* l) { return read_value<uint32_t>(l); }
static int gm_i8(lua_State* l) { return read_value<int8_t>(l); }
static int gm_i16(lua_State* l) { return read_value<int16_t>(l); }
static int gm_i32(lua_State* l) { return read_value<int32_t>(l); }
static int gm_i64(lua_State* l) { return read_value<int64_t>(l); }
static int gm_f32(lua_State* l) { return read_value<float>(l); }
static int gm_f64(lua_State* l) { return read_value<double>(l); }

bool write_mem(uintptr_t addr, const uint8_t* bytes, size_t n) {
    if (!g_allow_write.load(std::memory_order_relaxed)) {
        log_line("script write to " + hex_addr(addr, true) + " refused: allow_write is not set in v2probe.txt");
        return false;
    }
    if (!accessible(addr, n, true)) {
        log_line("script write to " + hex_addr(addr, true) + " (" + std::to_string(n) +
                 " bytes) refused: not writable memory");
        return false;
    }
    memcpy(reinterpret_cast<void*>(addr), bytes, n);
    std::string shown = to_hex(bytes, std::min<size_t>(n, 16));
    log_line("script WRITE " + hex_addr(addr, true) + " <- " + shown + (n > 16 ? "..." : "") + " (" +
             std::to_string(n) + " bytes)");
    return true;
}

// Values are truncated to the width (little-endian, as x86 stores them).
template <typename T>
int write_value(lua_State* l) {
    T v = static_cast<T>(static_cast<uint64_t>(to_i64(lua.tonumber(l, 2))));
    uint8_t b[sizeof(T)];
    memcpy(b, &v, sizeof v);
    lua.pushboolean(l, write_mem(arg(l, 1), b, sizeof b) ? 1 : 0);
    return 1;
}

static int gm_w8(lua_State* l) { return write_value<uint8_t>(l); }
static int gm_w16(lua_State* l) { return write_value<uint16_t>(l); }
static int gm_w32(lua_State* l) { return write_value<uint32_t>(l); }
static int gm_w64(lua_State* l) { return write_value<int64_t>(l); }

static int gm_wbytes(lua_State* l) {
    uintptr_t addr = arg(l, 1);
    auto b = arg_bytes(l, 2);
    lua.pushboolean(l, write_mem(addr, b.first, b.second) ? 1 : 0);
    return 1;
}

static int gm_bytes(lua_State* l) {
    size_t n = arg(l, 2);
    const uint8_t* b = mem(arg(l, 1), n);
    if (b) push_bytes(l, b, n);
    else lua.pushnil(l);
    return 1;
}

static int gm_hex(lua_State* l) {
    size_t n = arg(l, 2);
    const uint8_t* b = mem(arg(l, 1), n);
    if (b) {
        std::string s = to_hex(b, n);
        push_bytes(l, s.data(), s.size());
    } else {
        lua.pushnil(l);
    }
    return 1;
}

// Zero-terminated text at addr, at most max bytes, stopping at the end of readable memory.
// Returns false when the first byte is not readable.
bool c_text(uintptr_t addr, size_t max, size_t* out_len) {
    size_t len = 0;
    while (len < max) {
        // Check a page at a time, so text near the end of a region is still returned.
        size_t step = std::min<size_t>(0x1000 - ((addr + len) & 0xfff), max - len);
        const uint8_t* chunk = mem(addr + len, step);
        if (!chunk) return false;
        const void* z = memchr(chunk, 0, step);
        if (z) {
            *out_len = len + static_cast<size_t>(static_cast<const uint8_t*>(z) - chunk);
            return true;
        }
        len += step;
    }
    *out_len = max;
    return true;
}

static int gm_cstr(lua_State* l) {
    size_t max = arg(l, 2);
    max = max == 0 ? 256 : std::min(max, MAX_RANGE);
    uintptr_t addr = arg(l, 1);
    size_t len = 0;
    if (c_text(addr, max, &len)) push_bytes(l, reinterpret_cast<const void*>(addr), len);
    else lua.pushnil(l);
    return 1;
}

// MSVC std::string: 16 bytes of inline text or a pointer, then length and capacity.
static int gm_str(lua_State* l) {
    uintptr_t obj = arg(l, 1);
    const uint8_t* b = mem(obj, 24);
    if (b) {
        uint32_t ptr, len, cap;
        memcpy(&ptr, b, 4);
        memcpy(&len, b + 16, 4);
        memcpy(&cap, b + 20, 4);
        if (len <= cap && len <= 0x10000) {
            const uint8_t* t = mem(cap > 15 ? ptr : obj, std::max<size_t>(len, 1));
            if (t) {
                push_bytes(l, t, len);
                return 1;
            }
        }
    }
    lua.pushnil(l);
    return 1;
}

static int gm_readable(lua_State* l) {
    lua.pushboolean(l, accessible(arg(l, 1), std::max<size_t>(arg(l, 2), 1), false) ? 1 : 0);
    return 1;
}

// Every allocated heap block of the process as (address, size), sorted by address. Filled by
// gm.heapscan(). Asking Windows about one arbitrary pointer (HeapValidate/HeapSize) can fault
// inside ntdll when the pointer is not a block start, so the blocks are listed by walking the
// heaps instead, and lookups only search this table.
std::mutex g_heap_lock;
std::vector<std::pair<uintptr_t, size_t>> g_heap_blocks;

// gm.heapscan(): lists all heap blocks (each heap locked while walked). Returns the number
// of blocks and their total size.
static int gm_heapscan(lua_State* l) {
    std::vector<std::pair<uintptr_t, size_t>> blocks;
    HANDLE heaps[256];
    DWORD n = std::min<DWORD>(GetProcessHeaps(256, heaps), 256);
    for (DWORD i = 0; i < n; ++i) {
        if (!HeapLock(heaps[i])) continue; // a heap without locking cannot be walked safely
        PROCESS_HEAP_ENTRY entry;
        memset(&entry, 0, sizeof entry);
        while (HeapWalk(heaps[i], &entry)) {
            if ((entry.wFlags & PROCESS_HEAP_ENTRY_BUSY) && entry.lpData)
                blocks.emplace_back(reinterpret_cast<uintptr_t>(entry.lpData), static_cast<size_t>(entry.cbData));
        }
        HeapUnlock(heaps[i]);
    }
    std::sort(blocks.begin(), blocks.end());
    double total = 0;
    for (const auto& b : blocks) total += static_cast<double>(b.second);
    size_t count = blocks.size();
    {
        std::lock_guard<std::mutex> g(g_heap_lock);
        g_heap_blocks.swap(blocks);
    }
    lua.pushnumber(l, static_cast<double>(count));
    lua.pushnumber(l, total);
    return 2;
}

// gm.heapsize(addr): size of the heap block starting exactly at addr, or nil.
static int gm_heapsize(lua_State* l) {
    uintptr_t addr = arg(l, 1), start = 0;
    size_t size = 0;
    bool found;
    {
        std::lock_guard<std::mutex> g(g_heap_lock);
        found = find_block(g_heap_blocks, addr, true, &start, &size);
    }
    if (found) lua.pushnumber(l, static_cast<double>(size));
    else lua.pushnil(l);
    return 1;
}

// gm.heapblock(addr): start and size of the heap block that contains addr, or nil.
static int gm_heapblock(lua_State* l) {
    uintptr_t addr = arg(l, 1), start = 0;
    size_t size = 0;
    bool found;
    {
        std::lock_guard<std::mutex> g(g_heap_lock);
        found = find_block(g_heap_blocks, addr, false, &start, &size);
    }
    if (!found) {
        lua.pushnil(l);
        return 1;
    }
    lua.pushnumber(l, static_cast<double>(start));
    lua.pushnumber(l, static_cast<double>(size));
    return 2;
}

static int gm_watch(lua_State* l) {
    bool reads = lua.toboolean(l, 3) != 0;
    int slot = watch::watch(arg(l, 1), arg(l, 2), reads);
    if (slot >= 0) lua.pushnumber(l, slot);
    else lua.pushnil(l);
    return 1;
}

static int gm_unwatch(lua_State* l) {
    int slot = -1; // no argument: all slots
    if (lua.gettop(l) >= 1) {
        uintptr_t a = arg(l, 1);
        slot = a > static_cast<uintptr_t>(INT_MAX) ? INT_MAX : static_cast<int>(a);
    }
    watch::unwatch(slot);
    return 0;
}

static int gm_watch_hits(lua_State* l) {
    uintptr_t base = hooks::exe_base();
    std::vector<watch::Hit> hits = watch::take_hits();
    lua.createtable(l, static_cast<int>(hits.size()), 0);
    for (size_t i = 0; i < hits.size(); ++i) {
        const watch::Hit& h = hits[i];
        lua.createtable(l, 0, 4);
        lua.pushnumber(l, h.slot);
        lua.setfield(l, -2, "slot");
        lua.pushnumber(l, static_cast<double>(h.at - base));
        lua.setfield(l, -2, "rva");
        lua.pushnumber(l, static_cast<double>(h.count));
        lua.setfield(l, -2, "count");
        std::string text;
        for (size_t c = 0; c < h.callers.size(); ++c) {
            if (c) text += ' ';
            text += hex_addr(h.callers[c] - base, false);
        }
        push_bytes(l, text.data(), text.size());
        lua.setfield(l, -2, "callers");
        lua.rawseti(l, -2, static_cast<int>(i) + 1);
    }
    return 1;
}

static int gm_cover_load(lua_State* l) {
    auto p = arg_bytes(l, 1);
    std::string text;
    if (p.first && read_whole(utf8_to_wide(reinterpret_cast<const char*>(p.first), p.second), &text))
        lua.pushnumber(l, static_cast<double>(cover::load(hooks::exe_base(), cover::parse_list(text))));
    else
        lua.pushnil(l);
    return 1;
}

static int gm_cover_arm(lua_State* l) {
    size_t n = cover::arm();
    log_line("script: coverage armed on " + std::to_string(n) + " functions");
    lua.pushnumber(l, static_cast<double>(n));
    return 1;
}

static int gm_cover_disarm(lua_State* l) {
    lua.pushnumber(l, static_cast<double>(cover::disarm()));
    return 1;
}

static int gm_cover_hits(lua_State* l) {
    std::string s = cover::hits(hooks::exe_base());
    push_bytes(l, s.data(), s.size());
    return 1;
}

static int gm_base(lua_State* l) {
    lua.pushnumber(l, static_cast<double>(hooks::exe_base()));
    return 1;
}

static int gm_gs(lua_State* l) {
    lua.pushnumber(l, static_cast<double>(hooks::game_state()));
    return 1;
}

static int gm_tick(lua_State* l) {
    lua.pushnumber(l, static_cast<double>(hooks::ticks()));
    return 1;
}

static int gm_date(lua_State* l) {
    if (g_has_date.load(std::memory_order_relaxed)) lua.pushnumber(l, g_date.load(std::memory_order_relaxed));
    else lua.pushnil(l);
    return 1;
}

static int gm_dir(lua_State* l) {
    std::string s = dll_dir_utf8();
    push_bytes(l, s.data(), s.size());
    return 1;
}

static int gm_log(lua_State* l) {
    std::string line = "script: ";
    int n = lua.gettop(l);
    for (int i = 1; i <= n; ++i) {
        if (i > 1) line += '\t';
        line += value_text(l, i);
    }
    log_line(line);
    return 0;
}

static int gm_clock(lua_State* l) {
    std::chrono::duration<double> t = std::chrono::steady_clock::now() - g_start;
    lua.pushnumber(l, t.count());
    return 1;
}

// Files opened by the script; a handle is the slot number + 1.
std::mutex g_files_lock;
std::vector<FILE*> g_files;

static int gm_fopen(lua_State* l) {
    auto p = arg_bytes(l, 1);
    bool append = lua.toboolean(l, 2) != 0;
    std::wstring path = p.first ? utf8_to_wide(reinterpret_cast<const char*>(p.first), p.second) : std::wstring();
    FILE* f = nullptr;
    errno_t err = path.empty() ? ENOENT : _wfopen_s(&f, path.c_str(), append ? L"ab" : L"wb");
    if (err != 0 || !f) {
        char msg[256] = "cannot open file";
        if (err) strerror_s(msg, sizeof msg, err);
        lua.pushnil(l);
        push_bytes(l, msg, strlen(msg));
        return 2;
    }
    setvbuf(f, nullptr, _IOFBF, 1 << 20);
    size_t slot;
    {
        std::lock_guard<std::mutex> g(g_files_lock);
        slot = static_cast<size_t>(std::find(g_files.begin(), g_files.end(), nullptr) - g_files.begin());
        if (slot == g_files.size()) g_files.push_back(nullptr);
        g_files[slot] = f;
    }
    lua.pushnumber(l, static_cast<double>(slot + 1));
    return 1;
}

static int gm_fwrite(lua_State* l) {
    uintptr_t handle = arg(l, 1);
    bool ok = false;
    std::lock_guard<std::mutex> g(g_files_lock);
    if (handle >= 1 && handle <= g_files.size() && g_files[handle - 1]) {
        FILE* f = g_files[handle - 1];
        ok = true;
        int n = lua.gettop(l);
        for (int i = 2; i <= n && ok; ++i) {
            auto b = arg_bytes(l, i);
            if (b.second) ok = fwrite(b.first, 1, b.second, f) == b.second;
        }
    }
    lua.pushboolean(l, ok ? 1 : 0);
    return 1;
}

static int gm_fclose(lua_State* l) {
    uintptr_t handle = arg(l, 1);
    FILE* f = nullptr;
    {
        std::lock_guard<std::mutex> g(g_files_lock);
        if (handle >= 1 && handle <= g_files.size()) std::swap(f, g_files[handle - 1]);
    }
    bool ok = false;
    if (f) {
        ok = fflush(f) == 0;
        if (fclose(f) != 0) ok = false;
    }
    lua.pushboolean(l, ok ? 1 : 0);
    return 1;
}

static int gm_readfile(lua_State* l) {
    auto p = arg_bytes(l, 1);
    std::string data;
    if (p.first && read_whole(utf8_to_wide(reinterpret_cast<const char*>(p.first), p.second), &data))
        push_bytes(l, data.data(), data.size());
    else
        lua.pushnil(l);
    return 1;
}

static int gm_quit(lua_State*) {
    g_quit.store(true, std::memory_order_relaxed);
    return 0;
}

// Creates the script's Lua state with the standard libraries and the gm table.
lua_State* new_state() {
    lua_State* l = lua.newstate();
    if (!l) return l;
    lua.openlibs(l);
    static const struct {
        const char* name;
        lua_CFunction f;
    } functions[41] = {
        {"u8", gm_u8},
        {"u16", gm_u16},
        {"u32", gm_u32},
        {"ptr", gm_u32},
        {"i8", gm_i8},
        {"i16", gm_i16},
        {"i32", gm_i32},
        {"i64", gm_i64},
        {"f32", gm_f32},
        {"f64", gm_f64},
        {"bytes", gm_bytes},
        {"hex", gm_hex},
        {"cstr", gm_cstr},
        {"str", gm_str},
        {"readable", gm_readable},
        {"cover_load", gm_cover_load},
        {"cover_arm", gm_cover_arm},
        {"cover_disarm", gm_cover_disarm},
        {"cover_hits", gm_cover_hits},
        {"watch", gm_watch},
        {"unwatch", gm_unwatch},
        {"watch_hits", gm_watch_hits},
        {"heapscan", gm_heapscan},
        {"heapsize", gm_heapsize},
        {"heapblock", gm_heapblock},
        {"base", gm_base},
        {"gs", gm_gs},
        {"tick", gm_tick},
        {"date", gm_date},
        {"dir", gm_dir},
        {"log", gm_log},
        {"clock", gm_clock},
        {"fopen", gm_fopen},
        {"fwrite", gm_fwrite},
        {"fclose", gm_fclose},
        {"readfile", gm_readfile},
        {"quit", gm_quit},
        {"w8", gm_w8},
        {"w16", gm_w16},
        {"w32", gm_w32},
        {"w64", gm_w64},
    };
    lua.createtable(l, 0, 48);
    for (const auto& fn : functions) {
        lua.pushcclosure(l, fn.f, 0);
        lua.setfield(l, -2, fn.name);
    }
    lua.pushcclosure(l, gm_wbytes, 0);
    lua.setfield(l, -2, "wbytes");
    // gm.cfg: the numeric keys of v2probe.txt.
    lua.createtable(l, 0, 64);
    {
        std::lock_guard<std::mutex> g(g_config_lock);
        for (const auto& kv : g_config) {
            lua.pushnumber(l, static_cast<double>(kv.second));
            lua.setfield(l, -2, kv.first.c_str());
        }
    }
    lua.setfield(l, -2, "cfg");
    lua.setfield(l, LUA_GLOBALSINDEX, "gm");
    return l;
}

// Runs the script file in l. The file is read here (not by luaL_loadfile, which takes an ANSI
// path) so a folder name with any characters works. Like luaL_loadfile, a first line starting
// with '#' is skipped. Returns the Lua status (0 = fine; else the error is on the stack).
int run_file(lua_State* l, const std::wstring& path) {
    std::string code;
    std::string chunk = "@" + dll_dir_utf8() + SCRIPT_FILE;
    if (!read_whole(path, &code)) {
        std::string msg = "cannot read " + chunk.substr(1);
        lua.pushlstring(l, msg.data(), msg.size());
        return 1;
    }
    if (!code.empty() && code[0] == '#') {
        size_t eol = code.find('\n');
        code.erase(0, eol == std::string::npos ? code.size() : eol); // keep the newline: line numbers stay right
    }
    int status = lua.loadbuffer(l, code.data(), code.size(), chunk.c_str());
    if (status == 0) status = lua.pcall(l, 0, 0, 0);
    return status;
}

// Loads the script when it is new or changed on disk. Returns the Lua state, or null when
// there is no script.
lua_State* ensure_loaded() {
    std::wstring path = dll_dir() + SCRIPT_FILE_W;
    WIN32_FILE_ATTRIBUTE_DATA attr;
    bool exists = GetFileAttributesExW(path.c_str(), GetFileExInfoStandard, &attr) != 0;
    FILETIME now = exists ? attr.ftLastWriteTime : FILETIME{0, 0};
    std::lock_guard<std::mutex> g(g_loaded_lock);
    lua_State* l = g_state.load(std::memory_order_relaxed);
    if (g_loaded.checked && g_loaded.exists == exists && CompareFileTime(&g_loaded.time, &now) == 0)
        return exists ? l : nullptr;
    bool first = !g_loaded.checked;
    g_loaded.checked = true;
    g_loaded.exists = exists;
    g_loaded.time = now;
    if (!exists) {
        log_line(std::string("script: no ") + SCRIPT_FILE + "; scripting idle");
        return nullptr;
    }
    if (!l) {
        l = new_state();
        if (!l) {
            log_line("script: could not create a Lua state");
            return nullptr;
        }
        g_state.store(l, std::memory_order_relaxed);
    }
    if (run_file(l, path) != 0) {
        log_line(std::string("script: ") + SCRIPT_FILE + " failed to load: " + value_text(l, -1));
        lua.settop(l, 0);
    } else {
        log_line(std::string("script: ") + SCRIPT_FILE + (first ? " loaded" : " reloaded after a change"));
    }
    return l;
}

void call_inner(const char* name, uint64_t n, const int32_t* date) {
    lua_State* l = ensure_loaded();
    if (!l) return;
    lua.getfield(l, LUA_GLOBALSINDEX, name);
    if (lua.type(l, -1) != LUA_TFUNCTION) {
        lua.settop(l, 0);
        return;
    }
    lua.pushnumber(l, static_cast<double>(n));
    if (date) lua.pushnumber(l, *date);
    else lua.pushnil(l);
    if (lua.pcall(l, 2, 0, 0) != 0) log_line(std::string("script: ") + name + " failed: " + value_text(l, -1));
    lua.settop(l, 0);
}

} // namespace

// ---------------------------------------------------------------- the interface in proxy.h

namespace script {

// Keeps the hook configuration for gm.cfg. Called once from DllMain; touches no files.
const char* configure(const Config& config) {
    auto it = config.find("allow_write");
    bool write = it != config.end() && it->second != 0;
    g_allow_write.store(write, std::memory_order_relaxed);
    {
        std::lock_guard<std::mutex> g(g_config_lock);
        g_config = config;
    }
    return write ? ", script (writes allowed)" : ", script (read-only)";
}

// Calls the script's global function name(n, raw_date) if it exists (raw_date is nil when
// date_offset is not configured).
void call(const char* name, uint64_t n, const int32_t* raw_date) {
    // One caller at a time: the tick thread and the idle thread share the Lua state.
    std::lock_guard<std::mutex> turn(g_call_lock);
    g_date.store(raw_date ? *raw_date : 0, std::memory_order_relaxed);
    g_has_date.store(raw_date != nullptr, std::memory_order_relaxed);
#if defined(_M_IX86) || defined(__i386__)
    // Run the script with the x87 FPU set to double precision, then restore the previous
    // setting. The game's Direct3D device switches the FPU to single precision, which would
    // round every Lua number above 2^24 (all addresses) inside the script.
    unsigned int saved = _controlfp(0, 0);
    _controlfp(_PC_53, _MCW_PC);
    call_inner(name, n, raw_date);
    _controlfp(saved, _MCW_PC);
#else
    call_inner(name, n, raw_date);
#endif
}

// Starts the thread behind on_idle. Called from DllMain: the thread only starts running
// once the loader is done, and does nothing until the game state is set (or, without
// gamestate_ptr, until the first tick).
void start_idle_thread() {
    try {
        std::thread([] {
            for (;;) {
                std::this_thread::sleep_for(std::chrono::milliseconds(100));
                if (hooks::game_state() != 0 || hooks::ticks() > 0) {
                    int32_t date = 0;
                    bool has = hooks::raw_date(&date);
                    call("on_idle", hooks::ticks(), has ? &date : nullptr);
                    quit_if_requested();
                }
            }
        }).detach();
    } catch (...) {
        log_line("script: could not start the idle thread");
    }
}

// Ends the game if the script asked for it. Called at the very end of a tick.
void quit_if_requested() {
    if (g_quit.load(std::memory_order_relaxed)) {
        log_line("script: gm.quit() - ending the game process");
        TerminateProcess(GetCurrentProcess(), 0);
    }
}

} // namespace script
