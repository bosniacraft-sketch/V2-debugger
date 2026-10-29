# lua51.dll proxy (C++)

Victoria 2 loads Lua through `lua51.dll`, a thin forwarder to the real engine in `lua5.1.dll`.
This project builds a drop-in replacement: 124 exports are forwarded straight to `lua5.1.dll` by
the Windows loader (see `lua51.def`), and four are wrapped to log what the engine does with Lua. **Install it in a spare copy of the game**, and keep
the original as `lua51.dll.original` so you can put it back.

| Export | What the proxy adds |
| --- | --- |
| `luaL_openlibs` | keeps `io`/`os` reachable as `v2probe_io`/`v2probe_os` (the engine removes the originals) and registers `v2probe_log(...)` |
| `luaL_loadbuffer` | logs each chunk the engine loads (name, size) |
| `lua_getfield` | counts global-name lookups; logs each new name (shows which Lua hooks the engine calls) |
| `lua_pcall` | logs calls into Lua with duration, and errors |

Log: `v2probe_proxy.log` next to the DLL.

## Daily tick hook (optional)

`src/hooks.cpp` can redirect one vtable slot inside the running `v2game.exe`, in memory only (the
exe file is never modified), so your script runs once per game day. The proxy ships with **no
addresses**: you find them for your own build and write them in `v2probe.txt` next to the DLL.
Without that file no hook is installed. The hook first checks that the slot holds exactly the
expected value, so another game build is left alone.

```
# v2probe.txt: one key = value per line, hex (0x...) or decimal
tick_slot     = 0x...   # RVA of a vtable slot holding a method the game calls once per day
tick_expect   = 0x...   # RVA of the function in that slot now (sanity check)
gamestate_ptr = 0x...   # optional: RVA of a global pointer, returned by gm.gs()
date_offset   = 0x...   # optional: offset of a 32-bit value in that object, passed as raw_date
allow_write   = 0       # 1 lets scripts write game memory (every write is logged)
```

Any other numeric key you add is visible to scripts as `gm.cfg.<key>`.

## Scripting

`src/script.cpp` runs `v2probe_script.lua` from the DLL's folder in its own Lua state. It is
reloaded whenever the file changes, also while the game is running, so a new measurement is a
script edit, not a rebuild. The script may define:

- `before_tick(n, raw_date)` and `on_tick(n, raw_date)`: before and after the hooked daily method;
- `on_idle(n, raw_date)`: about ten times a second from a background thread, also while paused.

It gets a `gm` table with range-checked memory reads (`gm.u8` ... `gm.f64`, `gm.bytes`, `gm.hex`,
`gm.cstr`, `gm.str`), a heap-block table (`gm.heapscan`, `gm.heapsize`, `gm.heapblock`) for exact
object sizes, file output (`gm.fopen`, `gm.fwrite`, `gm.readfile`), `gm.log`, `gm.clock` and
`gm.quit()`. The full list is at the top of `src/script.cpp`. Writes (`gm.w8/w16/w32/w64/wbytes`)
work only with `allow_write = 1`.

Two research helpers:

- **Write-watch** (`src/watch.cpp`): `gm.watch(addr, len)` reports the code that writes a memory
  location, using the processor's debug registers. Nothing is patched.
- **Coverage** (`src/cover.cpp`): `gm.cover_load/arm/disarm/hits` record which functions from your
  own list run during a stretch of the game, and in what order, with one-shot breakpoints that
  remove themselves on first use.

Two things the game forces on this design: its Lua build has no `io`/`os` library (hence
`gm.fopen`/`gm.clock`), and its Direct3D device leaves the FPU in single precision, which rounds
Lua numbers above 2^24, so every script call runs with double precision restored.

## Build (Windows, 32-bit)

With Visual Studio 2019 or newer (the "Desktop development with C++" workload, which includes
CMake), double-click `build.bat`, or:

```cmd
cmake -B build -A Win32
cmake --build build --config Release
```

Output: `build\Release\lua51.dll`. With a 32-bit MinGW (i686, posix threads) use
`cmake -B build -G "MinGW Makefiles"` and `cmake --build build`; the DLL is `build\lua51.dll`.
Copy it over the game's `lua51.dll` (after keeping the original).

Tests: the `test_logic` target checks the pure helpers (config parsing, heap-block lookup, watch
slots, coverage lists) and builds on any host:

```sh
g++ -std=c++17 -Isrc tests/test_logic.cpp src/logic.cpp -o test_logic && ./test_logic
```

`lua51.def` is produced by `python gen_def.py` from `exports.list` (the original DLL's export
names). The sources include no Lua headers and contain no game code or data.
