// Pure helpers of the proxy: no Windows calls, so tests/test_logic.cpp runs them on any host.

#include <algorithm>
#include <cstdlib>
#include <sstream>

#include "proxy.h"

namespace {

const uint32_t MEM_COMMIT_ = 0x1000;
const uint32_t PAGE_NOACCESS_ = 0x01;
const uint32_t PAGE_GUARD_ = 0x100;
const uint32_t PAGE_WRITABLE_ = 0x04 | 0x08 | 0x40 | 0x80;

std::string trim(const std::string& s) {
    size_t a = s.find_first_not_of(" \t\r\n");
    if (a == std::string::npos) return "";
    size_t b = s.find_last_not_of(" \t\r\n");
    return s.substr(a, b - a + 1);
}

// Hex with or without 0x (hex = true), or decimal. False when the text is not a whole number.
bool parse_number(const std::string& text, bool hex, uintptr_t* out) {
    std::string t = text;
    if (t.size() > 2 && t[0] == '0' && (t[1] == 'x' || t[1] == 'X')) {
        t = t.substr(2);
        hex = true;
    }
    if (t.empty()) return false;
    char* end = nullptr;
    unsigned long long v = std::strtoull(t.c_str(), &end, hex ? 16 : 10);
    if (*end != '\0' || t[0] == '-' || t[0] == '+') return false;
    *out = static_cast<uintptr_t>(v);
    return true;
}

}  // namespace

Config parse_config(const std::string& text) {
    Config map;
    std::istringstream in(text);
    std::string line;
    while (std::getline(in, line)) {
        line = trim(line.substr(0, line.find('#')));
        size_t eq = line.find('=');
        if (eq == std::string::npos) continue;
        uintptr_t v = 0;
        if (parse_number(trim(line.substr(eq + 1)), false, &v)) map[trim(line.substr(0, eq))] = v;
    }
    return map;
}

std::string to_hex(const uint8_t* bytes, size_t n) {
    static const char DIGITS[] = "0123456789abcdef";
    std::string s;
    s.reserve(n * 2);
    for (size_t i = 0; i < n; i++) {
        s.push_back(DIGITS[bytes[i] >> 4]);
        s.push_back(DIGITS[bytes[i] & 15]);
    }
    return s;
}

bool region_ok(uint32_t state, uint32_t protect, bool write) {
    return state == MEM_COMMIT_ && protect != 0 && (protect & (PAGE_NOACCESS_ | PAGE_GUARD_)) == 0 &&
           (!write || (protect & PAGE_WRITABLE_) != 0);
}

bool find_block(const std::vector<std::pair<uintptr_t, size_t>>& blocks, uintptr_t addr, bool exact,
                uintptr_t* start, size_t* size) {
    auto it = std::upper_bound(blocks.begin(), blocks.end(), addr,
                               [](uintptr_t a, const std::pair<uintptr_t, size_t>& b) { return a < b.first; });
    if (it == blocks.begin()) return false;
    --it;
    bool hit = exact ? it->first == addr : addr - it->first < std::max<size_t>(it->second, 1);
    if (!hit) return false;
    *start = it->first;
    *size = it->second;
    return true;
}

namespace watch {

uint32_t dr7(const Slot (&slots)[4]) {
    uint32_t v = 0;
    for (uint32_t i = 0; i < 4; i++) {
        const Slot& s = slots[i];
        if (s.len == 0) continue;
        uint32_t access = s.reads ? 3 : 1;
        uint32_t len = s.len == 1 ? 0 : s.len == 2 ? 1 : 3;
        v |= 1u << (i * 2);
        v |= access << (16 + i * 4);
        v |= len << (18 + i * 4);
    }
    return v;
}

int add(Slot (&slots)[4], uintptr_t addr, size_t len, bool reads) {
    if ((len != 1 && len != 2 && len != 4) || addr == 0 || addr % len != 0) return -1;
    for (int i = 0; i < 4; i++) {
        if (slots[i].len == 0) {
            slots[i].addr = addr;
            slots[i].len = len;
            slots[i].reads = reads;
            return i;
        }
    }
    return -1;
}

}  // namespace watch

namespace cover {

std::vector<uintptr_t> parse_list(const std::string& text) {
    std::vector<uintptr_t> out;
    std::istringstream in(text);
    std::string line;
    while (std::getline(in, line)) {
        uintptr_t v = 0;
        if (parse_number(trim(line.substr(0, line.find('#'))), true, &v)) out.push_back(v);
    }
    std::sort(out.begin(), out.end());
    out.erase(std::unique(out.begin(), out.end()), out.end());
    return out;
}

}  // namespace cover
