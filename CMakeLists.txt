cmake_minimum_required(VERSION 3.22)
project(gatherer
    VERSION 0.1.0
    LANGUAGES C CXX
)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

if(APPLE)
    set(CMAKE_OSX_ARCHITECTURES "arm64;x86_64" CACHE STRING "Universal binary archs")
endif()

if(MSVC)
    add_compile_options(/W4 /permissive- /utf-8)
else()
    add_compile_options(-Wall -Wextra -Wpedantic)
endif()

option(BUILD_PLUGINS "Build the satellite and hub VST3 plugins (requires JUCE)" ON)

if(BUILD_PLUGINS)
    set(JUCE_PATH "/Users/felix/Documents/JUCE" CACHE PATH "Path to local JUCE source tree")
    if(NOT EXISTS "${JUCE_PATH}/CMakeLists.txt")
        message(FATAL_ERROR "BUILD_PLUGINS=ON but JUCE not found at JUCE_PATH=${JUCE_PATH}")
    endif()
    add_subdirectory("${JUCE_PATH}" "${CMAKE_BINARY_DIR}/JUCE")
endif()

add_subdirectory(common)
add_subdirectory(tests)
add_subdirectory(tools)

if(BUILD_PLUGINS)
    add_subdirectory(satellite)
    add_subdirectory(hub)
endif()
