# v2probe lua51.dll proxy for Victoria 2. 32-bit Windows only (v2game.exe is 32-bit).
#   Visual Studio:  cmake -B build -A Win32  &&  cmake --build build --config Release
#   MinGW (i686):   cmake -B build -G "MinGW Makefiles"  &&  cmake --build build
cmake_minimum_required(VERSION 3.16)
project(v2probe_lua51 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# The pure helpers and their tests build on any host.
add_executable(test_logic tests/test_logic.cpp src/logic.cpp)
target_include_directories(test_logic PRIVATE src)

if(WIN32)
    if(NOT CMAKE_SIZEOF_VOID_P EQUAL 4)
        message(FATAL_ERROR "Build the proxy as 32-bit: use -A Win32 (Visual Studio) or an i686 MinGW compiler.")
    endif()
    add_library(lua51 SHARED
        src/proxy.cpp src/hooks.cpp src/script.cpp src/watch.cpp src/cover.cpp src/logic.cpp)
    # Forwarded exports: Visual Studio reads them from src/forwards.h, MinGW from lua51.def.
    if(NOT MSVC)
        target_sources(lua51 PRIVATE lua51.def)
    endif()
    # The DLL must be called exactly lua51.dll, without a "lib" prefix.
    set_target_properties(lua51 PROPERTIES PREFIX "" OUTPUT_NAME "lua51")
    if(MSVC)
        target_compile_options(lua51 PRIVATE /W4 /permissive-)
        # Static runtime: the game folder needs no extra Visual C++ redistributable.
        set_property(TARGET lua51 PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
    else()
        target_compile_options(lua51 PRIVATE -Wall -Wextra)
        target_link_options(lua51 PRIVATE -static -static-libgcc -static-libstdc++)
    endif()
endif()
