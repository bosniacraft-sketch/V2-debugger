# lua51.dll proxy

Victoria 2 loads Lua through `lua51.dll`, a thin forwarder to the real engine in `lua5.1.dll`.
This crate builds a drop-in replacement: 124 exports jump straight to `lua5.1.dll`, and four are
wrapped to log what the engine does with Lua. **Install it in a spare copy of the game**, and keep
the original as `lua51.dll.original` so you can put it back.

| Export | What the proxy adds |
| --- | --- |
| `luaL_openlibs` | keeps `io`/`os` reachable as `v2probe_io`/`v2probe_os` (the engine removes the originals) and registers `v2probe_log(...)` |
| `luaL_loadbuffer` | logs each chunk the engine loads (name, size) |
| `lua_getfield` | counts global-name lookups; logs each new name (shows which Lua hooks the engine calls) |
| `lua_pcall` | logs calls into Lua with duration, and errors |

Log: `v2probe_proxy.log` next to the DLL.

## Daily tick hook (optional)

`src/hooks.rs` can redirect one vtable slot inside the running `v2game.exe`, in memory only (the
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

`src/script.rs` runs `v2probe_script.lua` from the DLL's folder in its own Lua state. It is
reloaded whenever the file changes, also while the game is running, so a new measurement is a
script edit, not a rebuild. The script may define:

- `before_tick(n, raw_date)` and `on_tick(n, raw_date)`: before and after the hooked daily method;
- `on_idle(n, raw_date)`: about ten times a second from a background thread, also while paused.

It gets a `gm` table with range-checked memory reads (`gm.u8` ... `gm.f64`, `gm.bytes`, `gm.hex`,
`gm.cstr`, `gm.str`), a heap-block table (`gm.heapscan`, `gm.heapsize`, `gm.heapblock`) for exact
object sizes, file output (`gm.fopen`, `gm.fwrite`, `gm.readfile`), `gm.log`, `gm.clock` and
`gm.quit()`. The full list is at the top of `src/script.rs`. Writes (`gm.w8/w16/w32/w64/wbytes`)
work only with `allow_write = 1`.

Two research helpers:

- **Write-watch** (`src/watch.rs`): `gm.watch(addr, len)` reports the code that writes a memory
  location, using the processor's debug registers. Nothing is patched.
- **Coverage** (`src/cover.rs`): `gm.cover_load/arm/disarm/hits` record which functions from your
  own list run during a stretch of the game, and in what order, with one-shot breakpoints that
  remove themselves on first use.

Two things the game forces on this design: its Lua build has no `io`/`os` library (hence
`gm.fopen`/`gm.clock`), and its Direct3D device leaves the FPU in single precision, which rounds
Lua numbers above 2^24, so every script call runs with double precision restored.

## Build (Windows, 32-bit)

```cmd
rustup target add i686-pc-windows-msvc
build.bat
```

Output: `target\i686-pc-windows-msvc\release\lua51.dll`. Copy it over the game's `lua51.dll`
(after keeping the original).

Tests: `cargo test` (also runs on a 64-bit host); on Windows, `cargo test --target
i686-pc-windows-msvc` with `V2PROBE_LUA_DLL` set to a 32-bit `lua5.1.dll` also runs the `gm`
functions in a real Lua state, and `V2PROBE_SCRIPT` compile-checks a script.

`src/generated.rs` is produced by `python gen.py` from `exports.list` (the original DLL's export
names). No dependencies; contains no game code or data.
