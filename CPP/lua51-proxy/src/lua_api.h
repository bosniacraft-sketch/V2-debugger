// The parts of the Lua 5.1 C API the proxy uses, called through pointers into the real
// lua5.1.dll (filled in by proxy.cpp before the game can call anything). No Lua headers needed.
#pragma once
#include <cstddef>

struct lua_State;
typedef int (*lua_CFunction)(lua_State*);

const int LUA_GLOBALSINDEX = -10002;
const int LUA_TNIL = 0;
const int LUA_TNUMBER = 3;
const int LUA_TSTRING = 4;
const int LUA_TFUNCTION = 6;

struct LuaApi {
    lua_State* (*newstate)();
    void (*openlibs)(lua_State*);
    int (*loadfile)(lua_State*, const char*);
    int (*loadstring)(lua_State*, const char*);
    int (*loadbuffer)(lua_State*, const char*, size_t, const char*);
    int (*pcall)(lua_State*, int, int, int);
    void (*getfield)(lua_State*, int, const char*);
    void (*setfield)(lua_State*, int, const char*);
    void (*pushcclosure)(lua_State*, lua_CFunction, int);
    void (*pushnumber)(lua_State*, double);
    void (*pushlstring)(lua_State*, const char*, size_t);
    void (*pushboolean)(lua_State*, int);
    void (*pushnil)(lua_State*);
    double (*tonumber)(lua_State*, int);
    const char* (*tolstring)(lua_State*, int, size_t*);
    int (*toboolean)(lua_State*, int);
    void (*settop)(lua_State*, int);
    int (*gettop)(lua_State*);
    int (*type)(lua_State*, int);
    const char* (*typeName)(lua_State*, int);
    void (*createtable)(lua_State*, int, int);
    void (*rawseti)(lua_State*, int, int);
};

// The real functions in lua5.1.dll. All pointers are set, or the DLL refuses to load.
extern LuaApi lua;
