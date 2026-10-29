// Shared declarations of the v2probe proxy. Each block names the file that defines it.
#pragma once
#include <cstddef>
#include <cstdint>
#include <map>
#include <string>
#include <utility>
#include <vector>

#include "lua_api.h"

using Config = std::map<std::string, uintptr_t>;

// ---------------------------------------------------------------- proxy.cpp
void log_line(const std::string& msg);      // appends a timestamped line to v2probe_proxy.log
std::wstring dll_dir();                      // folder of this DLL, with a trailing backslash
std::string dll_dir_utf8();                  // the same as UTF-8
std::string value_text(lua_State* l, int idx); // strings and numbers as text, else "<type>"

// ---------------------------------------------------------------- logic.cpp (no Windows calls; unit-tested on any host)
Config parse_config(const std::string& text);             // `key = value` lines, hex or decimal, # comments
std::string to_hex(const uint8_t* bytes, size_t n);
bool region_ok(uint32_t state, uint32_t protect, bool write);
// The block that starts exactly at addr (exact) or contains it. Blocks are sorted (start, size).
bool find_block(const std::vector<std::pair<uintptr_t, size_t>>& blocks, uintptr_t addr, bool exact,
                uintptr_t* start, size_t* size);
namespace watch {
struct Slot {
    uintptr_t addr = 0;
    size_t len = 0;     // 1, 2 or 4; 0 = free
    bool reads = false;
};
uint32_t dr7(const Slot (&slots)[4]);                        // DR7 control value for the slots
int add(Slot (&slots)[4], uintptr_t addr, size_t len, bool reads); // first free slot, or -1
}
namespace cover {
std::vector<uintptr_t> parse_list(const std::string& text);  // sorted, de-duplicated hex RVAs
}

// ---------------------------------------------------------------- hooks.cpp
namespace hooks {
bool read_config(const std::wstring& path, Config* out);    // false when the file is missing
std::string install(const Config& config);                 // status line for the log
uintptr_t exe_base();
uintptr_t game_state();                                    // 0 when not configured or not set
bool raw_date(int32_t* out);                               // false when not configured
uint64_t ticks();
}

// ---------------------------------------------------------------- script.cpp
namespace script {
const char* configure(const Config& config);               // keeps gm.cfg and allow_write
void call(const char* name, uint64_t n, const int32_t* raw_date); // raw_date may be null
void start_idle_thread();
void quit_if_requested();
}

// ---------------------------------------------------------------- watch.cpp
namespace watch {
struct Hit {
    int slot;
    uintptr_t at;               // address after the accessing instruction
    uint64_t count;
    std::vector<uintptr_t> callers;
};
int watch(uintptr_t addr, size_t len, bool reads);         // slot, or -1
void unwatch(int slot);                                    // -1 clears all
std::vector<Hit> take_hits();                              // most frequent first; clears the list
}

// ---------------------------------------------------------------- cover.cpp
namespace cover {
size_t load(uintptr_t base, const std::vector<uintptr_t>& rvas);
size_t arm();
size_t disarm();
std::string hits(uintptr_t base);                          // `order rva thread_id` lines, in hit order
}
