// v2probe: a lua51.dll proxy for Victoria 2 (install it in a spare copy of the game).
//
// Victoria 2 imports Lua from `lua51.dll`, which is itself a thin forwarder to the real engine in
// `lua5.1.dll`. This DLL replaces that forwarder: 124 exports are forwarded straight to the real
// functions by the loader (lua51.def for MinGW, forwards.h for Visual Studio), and four are wrapped
// to log what the engine does:
//
//   luaL_openlibs    after the real call, keeps `io`/`os` reachable as `v2probe_io`/`v2probe_os`
//                    (the engine removes `io` and `os` later) and registers `v2probe_log(...)`.
//   luaL_loadbuffer  logs every chunk the engine loads (name and size).
//   lua_getfield     counts global name lookups; new names are logged (reveals engine hooks).
//   lua_pcall        logs calls into Lua with duration, and any error message.
//
// Output: `v2probe_proxy.log` next to this DLL. 32-bit only (`v2game.exe` is 32-bit).

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <algorithm>
#include <chrono>
#include <cstdio>
#include <mutex>
#include <unordered_map>

#include "forwards.h"
#include "proxy.h"

LuaApi lua;

static HMODULE this_module = nullptr;
static std::string hook_status;

// ---------------------------------------------------------------- paths

std::wstring dll_dir() {
    wchar_t buf[1024];
    DWORD n = GetModuleFileNameW(this_module, buf, 1024);
    std::wstring p(buf, std::min<DWORD>(n, 1024));
    size_t slash = p.find_last_of(L"\\/");
    return slash == std::wstring::npos ? std::wstring() : p.substr(0, slash + 1);
}

std::string dll_dir_utf8() {
    std::wstring w = dll_dir();
    int n = WideCharToMultiByte(CP_UTF8, 0, w.c_str(), static_cast<int>(w.size()), nullptr, 0, nullptr, nullptr);
    std::string s(static_cast<size_t>(n), '\0');
    WideCharToMultiByte(CP_UTF8, 0, w.c_str(), static_cast<int>(w.size()), &s[0], n, nullptr, nullptr);
    return s;
}

// ---------------------------------------------------------------- logging

static std::mutex log_mutex;
static FILE* log_file = nullptr;
static const auto start_time = std::chrono::steady_clock::now();

void log_line(const std::string& msg) {
    std::lock_guard<std::mutex> guard(log_mutex);
    if (!log_file) {
        log_file = _wfopen((dll_dir() + L"v2probe_proxy.log").c_str(), L"ab");
        if (!log_file) return;
        std::fprintf(log_file, "==== v2probe lua51 proxy loaded\n");
        if (!hook_status.empty()) std::fprintf(log_file, "==== %s\n", hook_status.c_str());
    }
    long long ms = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - start_time).count();
    std::fprintf(log_file, "[%7lld.%03lld] %s\n", ms / 1000, ms % 1000, msg.c_str());
    std::fflush(log_file);
}

std::string value_text(lua_State* l, int idx) {
    int t = lua.type(l, idx);
    if (t == LUA_TSTRING || t == LUA_TNUMBER) {
        size_t len = 0;
        const char* p = lua.tolstring(l, idx, &len);
        return p ? std::string(p, len) : std::string();
    }
    const char* name = lua.typeName(l, t);
    return std::string("<") + (name ? name : "?") + ">";
}

static std::string ptr_text(const void* p) {
    char buf[32];
    std::snprintf(buf, sizeof buf, "%p", p);
    return buf;
}

// `v2probe_log(...)` for Lua code: writes all arguments, tab-separated, to the proxy log.
static int v2probe_log(lua_State* l) {
    std::string line = "lua: ";
    int n = lua.gettop(l);
    for (int i = 1; i <= n; i++) {
        if (i > 1) line += '\t';
        line += value_text(l, i);
    }
    log_line(line);
    return 0;
}

// ---------------------------------------------------------------- loading

template <typename T>
static bool resolve(HMODULE real, const char* name, T* out) {
    *out = reinterpret_cast<T>(reinterpret_cast<void*>(GetProcAddress(real, name)));
    return *out != nullptr;
}

static bool load_real(HMODULE real) {
    bool ok = true;
    ok &= resolve(real, "luaL_newstate", &lua.newstate);
    ok &= resolve(real, "luaL_openlibs", &lua.openlibs);
    ok &= resolve(real, "luaL_loadfile", &lua.loadfile);
    ok &= resolve(real, "luaL_loadstring", &lua.loadstring);
    ok &= resolve(real, "luaL_loadbuffer", &lua.loadbuffer);
    ok &= resolve(real, "lua_pcall", &lua.pcall);
    ok &= resolve(real, "lua_getfield", &lua.getfield);
    ok &= resolve(real, "lua_setfield", &lua.setfield);
    ok &= resolve(real, "lua_pushcclosure", &lua.pushcclosure);
    ok &= resolve(real, "lua_pushnumber", &lua.pushnumber);
    ok &= resolve(real, "lua_pushlstring", &lua.pushlstring);
    ok &= resolve(real, "lua_pushboolean", &lua.pushboolean);
    ok &= resolve(real, "lua_pushnil", &lua.pushnil);
    ok &= resolve(real, "lua_tonumber", &lua.tonumber);
    ok &= resolve(real, "lua_tolstring", &lua.tolstring);
    ok &= resolve(real, "lua_toboolean", &lua.toboolean);
    ok &= resolve(real, "lua_settop", &lua.settop);
    ok &= resolve(real, "lua_gettop", &lua.gettop);
    ok &= resolve(real, "lua_type", &lua.type);
    ok &= resolve(real, "lua_typename", &lua.typeName);
    ok &= resolve(real, "lua_createtable", &lua.createtable);
    ok &= resolve(real, "lua_rawseti", &lua.rawseti);
    return ok;
}

BOOL WINAPI DllMain(HINSTANCE module, DWORD reason, LPVOID) {
    if (reason != DLL_PROCESS_ATTACH) return TRUE;
    this_module = module;
    HMODULE real = LoadLibraryA("lua5.1.dll");
    if (!real || !load_real(real)) return FALSE;  // refuse to load rather than crash later
    // Optional exe hook: addresses come from your own config file.
    Config config;
    if (!hooks::read_config(dll_dir() + L"v2probe.txt", &config)) {
        hook_status = "no v2probe.txt: exe hooks off";
    } else {
        hook_status = hooks::install(config);
    }
    return TRUE;
}

// ---------------------------------------------------------------- wrapped exports

extern "C" {

__declspec(dllexport) void luaL_openlibs(lua_State* l) {
    lua.openlibs(l);
    // Keep the libraries the engine strips later reachable under our own names.
    lua.getfield(l, LUA_GLOBALSINDEX, "io");
    lua.setfield(l, LUA_GLOBALSINDEX, "v2probe_io");
    lua.getfield(l, LUA_GLOBALSINDEX, "os");
    lua.setfield(l, LUA_GLOBALSINDEX, "v2probe_os");
    lua.pushcclosure(l, v2probe_log, 0);
    lua.setfield(l, LUA_GLOBALSINDEX, "v2probe_log");
    log_line("luaL_openlibs(L=" + ptr_text(l) + "): libraries opened; v2probe_io, v2probe_os, v2probe_log installed");
}

__declspec(dllexport) int luaL_loadbuffer(lua_State* l, const char* buff, size_t size, const char* name) {
    int r = lua.loadbuffer(l, buff, size, name);
    log_line("luaL_loadbuffer(L=" + ptr_text(l) + ", " + std::to_string(size) + " bytes, name=" +
             (name ? name : "<null>") + ") -> " + std::to_string(r));
    return r;
}

}  // extern "C"

namespace {
std::mutex lookup_mutex;
std::unordered_map<std::string, uint64_t> lookup_counts;
uint64_t lookup_total = 0;
uint32_t lookup_summaries = 0;
std::mutex pcall_mutex;
uint64_t pcall_count = 0;
}  // namespace

extern "C" {

__declspec(dllexport) void lua_getfield(lua_State* l, int idx, const char* k) {
    if (idx == LUA_GLOBALSINDEX && k) {
        std::string name(k), report;
        bool first = false;
        {
            std::lock_guard<std::mutex> guard(lookup_mutex);
            lookup_total++;
            uint64_t& c = lookup_counts[name];
            first = ++c == 1;
            if (lookup_total % 5000 == 0 && lookup_summaries < 200) {
                lookup_summaries++;
                std::vector<std::pair<std::string, uint64_t>> v(lookup_counts.begin(), lookup_counts.end());
                std::sort(v.begin(), v.end(), [](const auto& a, const auto& b) {
                    return a.second != b.second ? a.second > b.second : a.first < b.first;
                });
                report = "global lookups: " + std::to_string(lookup_total) + " total, " +
                         std::to_string(lookup_counts.size()) + " names; top: ";
                for (size_t i = 0; i < v.size() && i < 25; i++) {
                    if (i) report += ", ";
                    report += v[i].first + "=" + std::to_string(v[i].second);
                }
            }
        }
        if (first) log_line("global lookup (new): " + name);
        if (!report.empty()) log_line(report);
    }
    lua.getfield(l, idx, k);
}

__declspec(dllexport) int lua_pcall(lua_State* l, int nargs, int nresults, int errfunc) {
    uint64_t n;
    {
        std::lock_guard<std::mutex> guard(pcall_mutex);
        n = ++pcall_count;
    }
    auto t = std::chrono::steady_clock::now();
    int r = lua.pcall(l, nargs, nresults, errfunc);
    if (r != 0) {
        log_line("lua_pcall #" + std::to_string(n) + " (L=" + ptr_text(l) + ", nargs=" + std::to_string(nargs) +
                 ") ERROR " + std::to_string(r) + ": " + value_text(l, -1));
    } else if (n <= 100 || n % 1000 == 0) {
        long long us = std::chrono::duration_cast<std::chrono::microseconds>(std::chrono::steady_clock::now() - t).count();
        log_line("lua_pcall #" + std::to_string(n) + " (L=" + ptr_text(l) + ", nargs=" + std::to_string(nargs) +
                 ", nresults=" + std::to_string(nresults) + ") ok in " + std::to_string(us) + " us");
    }
    return r;
}

}  // extern "C"
