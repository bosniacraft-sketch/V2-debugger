# Victoria 2 probing tools (C++ edition)

Small tools for modders and players who want to see what Victoria 2 (the 2010 Paradox game) is
actually doing while it runs, so they can debug their mods and their games. Made while building
[Wiktoria](https://github.com/bosniacraft-sketch/WIKTORIA). They contain no game code, no game data
and no addresses: you bring your own copy of the game.

This is the C++ edition: the proxy builds with Visual Studio or MinGW through CMake. A Rust
edition with the same features also exists.

**Always use a spare copy of the game**, never the one you play.

| Folder | What it is |
| --- | --- |
| `lua51-proxy/` | A drop-in `lua51.dll` (C++, CMake) that logs the engine's Lua use, and optionally runs your own Lua script once per game day with safe memory reads, a write-watch and a coverage recorder. See its [README](lua51-proxy/README.md). |
| `lua/probe.lua` | A read-only probe of the Lua environment the game exposes: globals, their members, and which global names the engine looks up. Works with or without the proxy. |
| `lua/mailbox.lua` | A proxy script that lets you drive the running game through plain text files (ping, read memory, quit, plus your own commands). |
| `lua/vector_logger.lua` | An example proxy script: logs a list of objects from the game's memory once per day, then closes the game. |
| `docs/x32dbg-logging-breakpoints.md` | How to log every call of a game function with x32dbg without stopping the game. |

## What they help with

- **"Is my Lua running at all?"** The proxy log lists every Lua chunk the game loads (with its load result) and
  the error message of every failed Lua call, so a broken script stops being a silent mystery.
- **"What can my Lua see?"** `probe.lua` lists every global the game's Lua offers, their members,
  and which names the engine asks for, so you know what is there before you write a line.
- **"What is the game doing day by day?"** With the daily tick hook, your own script runs once per
  game day and can write whatever it reads to a file, then close the game when the run is done.
- **"What changes this value?"** The write-watch reports the code that writes a memory location,
  and x32dbg logging breakpoints show every call of a function, both without stopping the game.

## Quick start

1. Make a copy of your Victoria 2 folder.
2. For the Lua probe: follow the comment at the top of `lua/probe.lua`.
3. For the proxy: build it (see `lua51-proxy/README.md`), keep the copy's `lua51.dll` as
   `lua51.dll.original`, and put the new one in its place. Start the game and read
   `v2probe_proxy.log` next to the DLL.
4. For scripts: add `v2probe.txt` with your own addresses, and copy a script from `lua/` next to
   the DLL as `v2probe_script.lua`.

## Licence

MIT, see [LICENSE](LICENSE). Victoria 2 is a trademark of Paradox Interactive; this project is
not affiliated with or endorsed by Paradox.
