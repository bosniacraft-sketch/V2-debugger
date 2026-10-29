-- probe.lua: read-only probe of the Lua environment Victoria 2 exposes.
-- Use a spare copy of the game. Add this line at the end of its common/defines.lua (the exe never
-- runs autoexec.lua), with the full path to this file:
--     if pcall and dofile then pcall(dofile, "C:\\Games\\Victoria2_test\\script\\probe.lua") end
-- Writes LOG below (set a full path if the log does not appear) and/or, with the lua51.dll proxy,
-- the proxy log.
-- Everything is wrapped in pcall so the game never sees an error.

local LOG = "v2probe_probe.log"

-- The engine removes io/os after opening the libraries. The lua51.dll proxy keeps them reachable
-- as v2probe_io/v2probe_os and adds v2probe_log; use those when present.
local io = io or v2probe_io
local os = os or v2probe_os
local glog = v2probe_log

local f = (io and io.open) and io.open(LOG, "a") or nil
if not f and not glog then return end

local function log(...)
    local parts = {}
    for i = 1, select("#", ...) do parts[#parts + 1] = tostring(select(i, ...)) end
    local line = table.concat(parts, " ")
    if f then
        f:write(line, "\n")
        f:flush()
    end
    if glog then glog("probe: " .. line) end
end

local function safe(fn, ...)
    local ok, err = pcall(fn, ...)
    if not ok then log("  [error] " .. tostring(err)) end
end

log("v2probe probe started", os and os.date("%Y-%m-%d %H:%M:%S") or "?", _VERSION, "file:", f ~= nil, "proxy log:", glog ~= nil)

-- 1. Globals, sorted by name, with type.
safe(function()
    local names = {}
    for k in pairs(_G) do names[#names + 1] = tostring(k) end
    table.sort(names)
    log("== globals (" .. #names .. ")")
    for _, k in ipairs(names) do
        local v = _G[k]
        log("  " .. k .. " : " .. type(v))
    end
end)

-- 2. Members of tables and userdata/class objects (luabind classes are userdata).
local function dump_members(name, obj)
    local seen = {}
    local function add(k, v, src)
        local key = tostring(k)
        if not seen[key] then
            seen[key] = true
            log("    " .. key .. " : " .. type(v) .. "  (" .. src .. ")")
        end
    end
    if type(obj) == "table" then
        for k, v in pairs(obj) do add(k, v, "table") end
    end
    local mt = getmetatable(obj)
    if type(mt) == "table" then
        for k, v in pairs(mt) do add(k, v, "metatable") end
        if type(mt.__index) == "table" then
            for k, v in pairs(mt.__index) do add(k, v, "__index") end
        end
    end
    -- luabind's class_info, if the engine registered it.
    if type(class_info) == "function" then
        local ok, info = pcall(class_info, obj)
        if ok and type(info) == "table" then
            log("    class_info.name = " .. tostring(info.name))
            if type(info.methods) == "table" then
                for k, v in pairs(info.methods) do add(k, v, "class_info.methods") end
            end
            if type(info.attributes) == "table" then
                for _, a in ipairs(info.attributes) do add(a, nil, "class_info.attributes") end
            end
        end
    end
end

safe(function()
    log("== members of non-standard globals")
    local std = {
        _G = true, _VERSION = true, assert = true, collectgarbage = true, coroutine = true, debug = true,
        dofile = true, error = true, gcinfo = true, getfenv = true, getmetatable = true, io = true,
        ipairs = true, load = true, loadfile = true, loadstring = true, math = true, module = true,
        newproxy = true, next = true, os = true, package = true, pairs = true, pcall = true, print = true,
        rawequal = true, rawget = true, rawset = true, require = true, select = true, setfenv = true,
        setmetatable = true, string = true, table = true, tonumber = true, tostring = true, type = true,
        unpack = true, xpcall = true,
    }
    local names = {}
    for k in pairs(_G) do if not std[k] then names[#names + 1] = tostring(k) end end
    table.sort(names)
    for _, k in ipairs(names) do
        local v = _G[k]
        if type(v) == "table" or type(v) == "userdata" then
            log("  " .. k .. " (" .. type(v) .. ")")
            safe(dump_members, k, v)
        end
    end
end)

-- 3. Which global names does the engine look up later (hooks it tries to call)?
--    Counts per name; a summary is written every 200 new lookups and on each new name.
safe(function()
    local counts, total, summaries = {}, 0, 0
    local old_mt = getmetatable(_G) or {}
    local old_index = old_mt.__index
    setmetatable(_G, {
        __index = function(t, k)
            total = total + 1
            local key = tostring(k)
            if not counts[key] then
                counts[key] = 0
                log("lookup (new) " .. key)
            end
            counts[key] = counts[key] + 1
            -- Summaries every 200 lookups, at most 100 of them, so the log stays small.
            if total % 200 == 0 and summaries < 100 then
                summaries = summaries + 1
                local parts = {}
                for name, n in pairs(counts) do parts[#parts + 1] = name .. "=" .. n end
                table.sort(parts)
                log("lookup counts after " .. total .. ": " .. table.concat(parts, ", "))
            end
            if type(old_index) == "function" then return old_index(t, k) end
            if type(old_index) == "table" then return old_index[k] end
            return nil
        end,
        __newindex = old_mt.__newindex,
    })
    log("== lookup tracer installed")
end)

log("v2probe probe finished setup")
