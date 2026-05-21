add_library(gatherer-common STATIC
    shm/SharedMemory.cpp
)

target_include_directories(gatherer-common
    PUBLIC
        ${CMAKE_CURRENT_SOURCE_DIR}
)

if(UNIX AND NOT APPLE)
    target_link_libraries(gatherer-common PUBLIC rt)
endif()

# ---------------------------------------------------------------------------
# libebur128 — EBU R128 loudness measurement (BSD-2-Clause)
# Vendored via FetchContent so we get the C library compiled into our static
# pipeline; no system dependency at runtime.
# ---------------------------------------------------------------------------
include(FetchContent)

# Disable libebur128's own examples / extra targets.
set(BUILD_STATIC_LIBS ON  CACHE INTERNAL "")
set(BUILD_SHARED_LIBS OFF CACHE INTERNAL "")
set(ENABLE_TESTS      OFF CACHE INTERNAL "")
set(ENABLE_INTERNAL_QUEUE_H ON CACHE INTERNAL "")

FetchContent_Declare(
    libebur128
    GIT_REPOSITORY https://github.com/jiixyj/libebur128.git
    GIT_TAG        v1.2.6
)
# libebur128's top-level CMakeLists predates modern CMake's minimum-version
# enforcement; allow it to use legacy policies.
set(CMAKE_POLICY_VERSION_MINIMUM 3.5 CACHE INTERNAL "")
FetchContent_MakeAvailable(libebur128)

# libebur128 exports the `ebur128` target. Link it into our common lib and
# expose its include directories for downstream users.
target_link_libraries(gatherer-common PUBLIC ebur128)
target_include_directories(gatherer-common PUBLIC
    ${libebur128_SOURCE_DIR}/ebur128
)
