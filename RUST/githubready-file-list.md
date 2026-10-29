# GITHUB V2 MODDING TOOLS: file list (20 files)

Folder: `D:\DEVETAK ASSEMBLY\GREAT TOMB OF FENT\GITHUB V2 MODDING TOOLS\githubready` (one commit on `main`, not pushed).

## What changed from our own copies
- **lua51-proxy:** removed the production dump (rgodump.rs), the price log, the modifier dump, the daily autosaves and the game's date decoding. The tick hook stays, with no addresses: users supply their own in `v2probe.txt`. Renamed gamemix to v2probe.
- **probe.lua:** our folder path and evidence id removed.
- **mailbox.lua:** rewritten as a standalone script. Pause, speed, save and snapshots are gone (they used our offsets); users can add their own commands.
- **market case:** the script and manual held the market-order research (an offset and our notes), so they became a generic `vector_logger.lua` example and a generic x32dbg logging guide.
- **Leak check:** check_clean's patterns plus FENT, gamemix, workbook and note names: clean (one false hit, a hex test value). Tests: 6 passed.

## Files
```
.gitattributes
.gitignore
LICENSE                              MIT, same as Wiktoria
README.md
docs/x32dbg-logging-breakpoints.md
lua/mailbox.lua
lua/probe.lua
lua/vector_logger.lua
lua51-proxy/Cargo.lock
lua51-proxy/Cargo.toml
lua51-proxy/README.md
lua51-proxy/build.bat
lua51-proxy/exports.list
lua51-proxy/gen.py
lua51-proxy/src/cover.rs
lua51-proxy/src/generated.rs
lua51-proxy/src/hooks.rs
lua51-proxy/src/lib.rs
lua51-proxy/src/script.rs
lua51-proxy/src/watch.rs
```
