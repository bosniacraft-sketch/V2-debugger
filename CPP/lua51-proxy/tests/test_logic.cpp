// Unit tests of logic.cpp. Build and run on any host, for example:
//   g++ -std=c++17 -I../src test_logic.cpp ../src/logic.cpp -o test_logic && ./test_logic
// (CMake adds it as the `test_logic` target.)

#include <cstdio>
#include <cstdlib>

#include "proxy.h"

static int failures = 0;
#define CHECK(cond)                                                     \
    do {                                                                \
        if (!(cond)) {                                                  \
            std::printf("FAILED %s:%d: %s\n", __FILE__, __LINE__, #cond); \
            failures++;                                                 \
        }                                                               \
    } while (0)

static void parses_config() {
    Config c = parse_config("# comment\ntick_slot = 0x1A2b\ndate_offset=40 # trailing\nbad line\nx = -3\n");
    CHECK(c.size() == 2);
    CHECK(c["tick_slot"] == 0x1a2b);
    CHECK(c["date_offset"] == 40);
}

static void hex_text() {
    const uint8_t b[] = {0x00, 0x7f, 0xa5, 0xff};
    CHECK(to_hex(b, 4) == "007fa5ff");
    CHECK(to_hex(b, 0).empty());
}

static void finds_heap_blocks() {
    std::vector<std::pair<uintptr_t, size_t>> blocks = {{100, 16}, {200, 0}, {300, 8}};
    uintptr_t s = 0;
    size_t n = 0;
    CHECK(find_block(blocks, 100, true, &s, &n) && s == 100 && n == 16);
    CHECK(!find_block(blocks, 104, true, &s, &n));
    CHECK(find_block(blocks, 115, false, &s, &n) && s == 100);
    CHECK(!find_block(blocks, 116, false, &s, &n));
    CHECK(find_block(blocks, 200, true, &s, &n) && s == 200 && n == 0);
    CHECK(!find_block(blocks, 99, false, &s, &n));
    CHECK(!find_block({}, 5, false, &s, &n));
}

static void region_rules() {
    const uint32_t COMMIT = 0x1000, RW = 0x04, RO = 0x02;
    CHECK(region_ok(COMMIT, RO, false));
    CHECK(!region_ok(COMMIT, RO, true));
    CHECK(region_ok(COMMIT, RW, true));
    CHECK(!region_ok(0x2000, RW, false));
    CHECK(!region_ok(COMMIT, RW | 0x100, false));
    CHECK(!region_ok(COMMIT, 0x01, false));
}

static void slot_rules() {
    watch::Slot s[4];
    CHECK(watch::add(s, 0x1000, 4, false) == 0);
    CHECK(watch::add(s, 0x1002, 2, true) == 1);
    CHECK(watch::add(s, 0x1001, 2, false) == -1);  // address must be aligned to the length
    CHECK(watch::add(s, 0x1000, 3, false) == -1);  // length must be 1, 2 or 4
    CHECK(watch::add(s, 0x1003, 1, false) == 2);
    CHECK(watch::add(s, 0x2000, 4, false) == 3);
    CHECK(watch::add(s, 0x3000, 4, false) == -1);  // only four slots
    CHECK(watch::dr7(s) == (0x55u | (0xDu << 16) | (0x7u << 20) | (0x1u << 24) | (0xDu << 28)));
    watch::Slot empty[4];
    CHECK(watch::dr7(empty) == 0);
}

static void parses_lists() {
    std::vector<uintptr_t> v = cover::parse_list("# comment\n1f0\n0x20 # entry\n\n1f0\nzz\n10\n");
    CHECK((v == std::vector<uintptr_t>{0x10, 0x20, 0x1f0}));
    CHECK(cover::parse_list("").empty());
}

int main() {
    parses_config();
    hex_text();
    finds_heap_blocks();
    region_rules();
    slot_rules();
    parses_lists();
    if (failures) return 1;
    std::printf("all tests passed\n");
    return 0;
}
