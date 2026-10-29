-- vector_logger.lua: an example script for the proxy. Copy it next to lua51.dll as
-- v2probe_script.lua. Needs the proxy's tick hook and `gamestate_ptr` in v2probe.txt.
--
-- Each daily tick it reads an MSVC std::vector of pointers (a begin and an end pointer) at an
-- offset you choose inside the game state, and writes one line per day to
-- v2probe_vector_log.txt: the tick, the raw date, the length and, for each entry, the 32-bit value
-- at ENTRY_FIELD inside the object it points to (or the pointer itself when ENTRY_FIELD is nil).
-- After QUIT_TICK ticks it closes the game, so a run can be left alone.
--
-- Fill in the two offsets for your own game build, or put them in v2probe.txt as
-- `vector_offset = 0x...` and `entry_field = 0x...`.

local VECTOR_OFFSET = gm.cfg.vector_offset   -- where the begin/end pointers sit in the game state
local ENTRY_FIELD   = gm.cfg.entry_field     -- optional: a field to read from each entry
local DAYS          = 3
local QUIT_TICK     = 4
local MAX_ENTRIES   = 4096

local function log_vector(n, raw_date)
  local gs = gm.gs()
  if gs == 0 or not VECTOR_OFFSET then return end
  local f = gm.fopen(gm.dir() .. "v2probe_vector_log.txt", n > 1)
  if not f then gm.log("vector logger: cannot write the log"); return end
  local b, e = gm.u32(gs + VECTOR_OFFSET), gm.u32(gs + VECTOR_OFFSET + 4)
  if not b or not e or b == 0 or e < b or (e - b) / 4 > MAX_ENTRIES then
    gm.fwrite(f, string.format("tick %d date %s: no vector\n", n, tostring(raw_date))); gm.fclose(f); return
  end
  local out = {}
  for i = 0, (e - b) / 4 - 1 do
    local p = gm.u32(b + i * 4)
    if ENTRY_FIELD then
      out[#out + 1] = tostring(p and p ~= 0 and gm.u32(p + ENTRY_FIELD) or -1)
    else
      out[#out + 1] = string.format("%x", p or 0)
    end
  end
  gm.fwrite(f, string.format("tick %d date %s length %d: %s\n", n, tostring(raw_date), #out, table.concat(out, " ")))
  gm.fclose(f)
end

function on_tick(n, raw_date)
  if n <= DAYS then log_vector(n, raw_date) end
  if n >= QUIT_TICK then gm.log("vector logger: done, closing the game"); gm.quit() end
end

gm.log("vector logger ready: DAYS=" .. DAYS .. " QUIT_TICK=" .. QUIT_TICK)
