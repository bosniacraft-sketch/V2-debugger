-- mailbox.lua: drive a running game through plain files. Copy it next to lua51.dll as
-- v2probe_script.lua (or paste it into your own script). Needs the proxy's tick hook (v2probe.txt).
--
-- Commands go into v2probe_mailbox\in.txt, one per line: "<number> <command> [arguments]". Every
-- command whose number is higher than the last one done runs once, in order; its answer goes to
-- v2probe_mailbox\out\<number>.txt and a line to v2probe_mailbox\done.txt. Nothing is ever
-- deleted, so numbers only go up. Create the folder v2probe_mailbox\out before starting.
-- Set MAILBOX = false to switch it off.
--
-- Built-in commands:
--   ping                      tick, raw date and clock
--   read <hexaddr> <bytes>    raw bytes as hex (at most 4096)
--   u32 <hexaddr>             one 32-bit value
--   w32 <hexaddr> <value>     writes one 32-bit value (needs allow_write = 1, logged)
--   quit                      ends the game
--
-- Add your own commands to COMMANDS below. Each gets up to two text arguments and returns the
-- answer as text. For example, once you know where your build keeps the game speed, write that
-- down in v2probe.txt as `my_speed = 0x...` and add:
--   COMMANDS.speed = function(k) gm.w32(gm.gs() + gm.cfg.my_speed, tonumber(k)); return "speed " .. k end

local MAILBOX = true
local mb = { dir = gm.dir() .. "v2probe_mailbox\\", last = nil, checked = 0 }

COMMANDS = COMMANDS or {}

local function hexnum(s) return s and tonumber((s:gsub("^0[xX]", "")), 16) end

COMMANDS.ping = function()
  return string.format("tick %d raw_date %s clock %.1f", gm.tick(), tostring(gm.date()), gm.clock())
end
COMMANDS.read = function(a, n)
  a, n = hexnum(a), math.min(tonumber(n or "0") or 0, 4096)
  if not a or n <= 0 then return "read needs <hexaddr> <bytes>" end
  return gm.hex(a, n) or "unreadable"
end
COMMANDS.u32 = function(a)
  a = hexnum(a)
  if not a then return "u32 needs <hexaddr>" end
  return tostring(gm.u32(a))
end
COMMANDS.w32 = function(a, v)
  a, v = hexnum(a), tonumber(v or "")
  if not a or not v then return "w32 needs <hexaddr> <value>" end
  return gm.w32(a, v) and "written" or "refused (allow_write off, or unreadable)"
end
COMMANDS.quit = function() gm.quit(); return "quitting" end

local function mb_last()
  if mb.last then return mb.last end
  local done = gm.readfile(mb.dir .. "done.txt") or ""
  local last = 0
  for k in done:gmatch("(%d+) ") do last = math.max(last, tonumber(k)) end
  mb.last = last
  return last
end

local function mb_answer(id, cmd, text)
  local f = gm.fopen(mb.dir .. "out\\" .. id .. ".txt")
  if f then gm.fwrite(f, text, "\n"); gm.fclose(f) end
  local d = gm.fopen(mb.dir .. "done.txt", true)
  if d then gm.fwrite(d, string.format("%d %s %.1f\n", id, cmd, gm.clock())); gm.fclose(d) end
  mb.last = id
end

local function mb_run(cmd, a1, a2)
  local f = COMMANDS[cmd]
  if not f then return "unknown command: " .. tostring(cmd) end
  return tostring(f(a1, a2))
end

local function mb_poll()
  if not MAILBOX then return end
  local now = gm.clock()
  if now - mb.checked < 0.3 then return end
  mb.checked = now
  local text = gm.readfile(mb.dir .. "in.txt")
  if not text then return end
  local last = mb_last()
  for line in text:gmatch("[^\r\n]+") do
    local id, cmd, a1, a2 = line:match("^%s*(%d+)%s+(%S+)%s*(%S*)%s*(%S*)")
    id = tonumber(id)
    if id and id > last then
      local ok, answer = pcall(mb_run, cmd, a1 ~= "" and a1 or nil, a2 ~= "" and a2 or nil)
      mb_answer(id, cmd, ok and answer or ("error: " .. tostring(answer)))
      last = id
    end
  end
end

-- Chains onto an on_idle defined earlier in the same script, if any.
local mb_old_idle = on_idle
function on_idle(n, raw_date)
  if mb_old_idle then mb_old_idle(n, raw_date) end
  pcall(mb_poll)
end

gm.log("mailbox ready: " .. mb.dir)
