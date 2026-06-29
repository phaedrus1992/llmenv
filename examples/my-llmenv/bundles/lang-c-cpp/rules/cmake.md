---
paths:
  - "**/CMakeLists.txt"
  - "**/*.cmake"
  - "**/CMakePresets.json"
---

# Modern CMake Conventions (3.15+)

## Minimum Version and Project Declaration

```cmake
cmake_minimum_required(VERSION 3.15...4.0)
project(MyProject VERSION 1.2.3 LANGUAGES CXX C)
```

- Version range `3.15...4.0` tells CMake to use the newest policies it knows while still
  accepting older installs down to 3.15.
- Always specify `LANGUAGES` explicitly — omitting it enables C and C++ by default, which
  compiles unnecessary checks for unused languages.
- Always set `VERSION` in `project()` — it populates `PROJECT_VERSION_*` variables used by
  `configure_file()` and packaging.

## Think in Targets, Not Directories

Modern CMake's central abstraction is the **target**. A target encapsulates everything needed
to build and use a library or executable. Avoid all commands that operate globally:

| Old (bad) | Modern replacement |
|-----------|-------------------|
| `include_directories()` | `target_include_directories()` |
| `link_directories()` | `target_link_libraries()` to a target |
| `add_definitions()` | `target_compile_definitions()` |
| `add_compile_options()` | `target_compile_options()` |

## PUBLIC / PRIVATE / INTERFACE — Always Explicit

Every `target_*` command requires a visibility keyword. Never omit it.

| Keyword | Meaning |
|---------|---------|
| `PRIVATE` | Only this target needs it. Not propagated to consumers. |
| `PUBLIC` | This target and all consumers need it. Propagated. |
| `INTERFACE` | Only consumers need it (e.g., header-only library). |

```cmake
add_library(mylib STATIC src/mylib.cpp)
target_include_directories(mylib
    PUBLIC  include          # consumers get this include path
    PRIVATE src              # internal only
)
target_compile_options(mylib
    PRIVATE -Wall -Wextra    # PRIVATE: don't impose warnings on consumers
)
target_link_libraries(myapp PRIVATE mylib)
```

**Do not make warning flags `PUBLIC` or `INTERFACE`.** Warning flags are your implementation
detail. Forcing them onto consumers breaks their builds.

## ALIAS Targets and Namespacing

Create `ALIAS` targets for all libraries. This makes `add_subdirectory()` and `find_package()`
usage identical for consumers.

```cmake
add_library(mylib STATIC ...)
add_library(MyLib::mylib ALIAS mylib)

# Consumer always writes:
target_link_libraries(consumer PRIVATE MyLib::mylib)
# — whether they used add_subdirectory() or find_package(MyLib)
```

## No `file(GLOB)` Without `CONFIGURE_DEPENDS`

Bare `file(GLOB ...)` is cached at configure time — adding a source file silently breaks
incremental builds until CMake is manually re-run.

```cmake
# bad
file(GLOB SOURCES src/*.cpp)

# acceptable (CMake 3.12+) — triggers reconfigure on file addition/removal
file(GLOB SOURCES CONFIGURE_DEPENDS src/*.cpp)

# best — list sources explicitly (always unambiguous)
add_library(mylib STATIC
    src/foo.cpp
    src/bar.cpp
)
```

## Out-of-Source Builds — Always

Never build inside the source tree. Enforce it:

```cmake
# At the top of the root CMakeLists.txt
if(CMAKE_SOURCE_DIR STREQUAL CMAKE_BINARY_DIR)
    message(FATAL_ERROR "In-source builds are not allowed. Use a separate build directory.")
endif()
```

Build convention: `cmake -B build -S .` or via presets.

## CMakePresets.json (CMake 3.19+)

Commit a `CMakePresets.json` at the repo root to make configurations reproducible and
shareable. Developers run `cmake --preset <name>` instead of remembering flags.

```json
{
  "version": 6,
  "configurePresets": [
    {
      "name": "default",
      "binaryDir": "${sourceDir}/build/${presetName}",
      "cacheVariables": {
        "CMAKE_BUILD_TYPE": "Debug",
        "BUILD_TESTING": "ON"
      }
    },
    {
      "name": "release",
      "inherits": "default",
      "cacheVariables": {
        "CMAKE_BUILD_TYPE": "Release",
        "BUILD_TESTING": "OFF"
      }
    },
    {
      "name": "asan",
      "inherits": "default",
      "cacheVariables": {
        "CMAKE_CXX_FLAGS": "-fsanitize=address,undefined",
        "CMAKE_EXE_LINKER_FLAGS": "-fsanitize=address,undefined"
      }
    }
  ]
}
```

## Generator Expressions for Config-Specific Logic

`if()` runs at configure time. For multi-config generators (Visual Studio, Xcode), use
generator expressions `$<...>` which evaluate at build time.

```cmake
# bad — broken for multi-config generators
if(CMAKE_BUILD_TYPE STREQUAL "Debug")
    target_compile_options(mylib PRIVATE -g3)
endif()

# good — correct for all generators
target_compile_options(mylib PRIVATE "$<$<CONFIG:Debug>:-g3>")

# useful patterns
target_compile_options(mylib PRIVATE
    "$<$<CXX_COMPILER_ID:GNU,Clang>:-Wall;-Wextra>"
    "$<$<CXX_COMPILER_ID:MSVC>:/W4>"
)
```

## Setting C++ Standard

```cmake
# per-target (preferred)
target_compile_features(mylib PUBLIC cxx_std_17)

# or project-wide baseline
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)   # -std=c++17, not -std=gnu++17
```

`cxx_std_17` as `PUBLIC` propagates the requirement to consumers automatically.
`CMAKE_CXX_EXTENSIONS OFF` prevents compiler-specific extensions.

## Dependency Management

**Priority order:**

1. `find_package()` for system-installed or vcpkg/Conan-managed libraries.
2. `FetchContent` for header-only or tightly-coupled in-tree dependencies.
3. Git submodules + `add_subdirectory()` when you need to patch the dependency.

### `find_package`

```cmake
find_package(OpenSSL 3.0 REQUIRED)
target_link_libraries(myapp PRIVATE OpenSSL::SSL OpenSSL::Crypto)
```

Always link to the imported target (`OpenSSL::SSL`), never to variables like
`${OPENSSL_LIBRARIES}`. Targets carry transitive dependencies; variables don't.

### `FetchContent` (CMake 3.11+)

```cmake
include(FetchContent)
FetchContent_Declare(
    catch2
    GIT_REPOSITORY https://github.com/catchorg/Catch2.git
    GIT_TAG        v3.6.0          # pin to a tag, never a branch
)
FetchContent_MakeAvailable(catch2)
```

Always pin to a specific tag or commit hash — never `main` or `HEAD`.

## Testing with CTest

```cmake
# top-level CMakeLists.txt
include(CTest)            # creates BUILD_TESTING option, defaults ON

if(BUILD_TESTING)
    add_subdirectory(tests)
endif()

# tests/CMakeLists.txt
add_executable(unit_tests test_foo.cpp)
target_link_libraries(unit_tests PRIVATE mylib Catch2::Catch2WithMain)

include(Catch)
catch_discover_tests(unit_tests)   # auto-registers each TEST_CASE with CTest
```

Run with: `ctest --test-dir build -j$(nproc) --output-on-failure`

## Build-Time Version Header

Pass version info from CMake into C++ via `configure_file()`:

```cmake
# Version.h.in
#pragma once
#define MY_VERSION_MAJOR @PROJECT_VERSION_MAJOR@
#define MY_VERSION_MINOR @PROJECT_VERSION_MINOR@
#define MY_VERSION       "@PROJECT_VERSION@"
```

```cmake
configure_file(include/Version.h.in "${CMAKE_CURRENT_BINARY_DIR}/include/Version.h")
target_include_directories(mylib PUBLIC "${CMAKE_CURRENT_BINARY_DIR}/include")
```

## Functions over Macros

`function()` creates its own variable scope; `macro()` does not. Prefer `function()` to avoid
polluting the caller's scope.

```cmake
function(add_strict_target TARGET)
    target_compile_options(${TARGET} PRIVATE -Wall -Wextra -Wpedantic -Werror)
    target_compile_features(${TARGET} PUBLIC cxx_std_17)
endfunction()
```

## Installing and Exporting for Consumers

Libraries intended for use by other projects must generate a `Config.cmake` file:

```cmake
include(GNUInstallDirs)
include(CMakePackageConfigHelpers)

install(TARGETS mylib
    EXPORT MyLibTargets
    ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR}
    INCLUDES DESTINATION ${CMAKE_INSTALL_INCLUDEDIR}
)
install(DIRECTORY include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})

install(EXPORT MyLibTargets
    FILE MyLibTargets.cmake
    NAMESPACE MyLib::
    DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/MyLib
)

write_basic_package_version_file(
    "${CMAKE_CURRENT_BINARY_DIR}/MyLibConfigVersion.cmake"
    VERSION ${PROJECT_VERSION}
    COMPATIBILITY SameMajorVersion
)

install(FILES MyLibConfig.cmake
    "${CMAKE_CURRENT_BINARY_DIR}/MyLibConfigVersion.cmake"
    DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/MyLib
)
```

**Do not write `FindMyLib.cmake`** — that's for third-party libraries that don't support CMake
natively. If you own the library, generate a proper `Config.cmake`.

## Antipatterns Checklist

| Antipattern | Fix |
|-------------|-----|
| `include_directories()` globally | `target_include_directories()` with scope |
| `link_libraries()` globally | `target_link_libraries()` with scope |
| Warning flags as `PUBLIC` | Make them `PRIVATE` |
| `file(GLOB)` without `CONFIGURE_DEPENDS` | Explicit source list or add `CONFIGURE_DEPENDS` |
| `cmake_minimum_required(VERSION 2.8)` | Update to `3.15...4.0` minimum |
| `if(CMAKE_BUILD_TYPE STREQUAL "Debug")` | Generator expression `$<$<CONFIG:Debug>:...>` |
| `find_package` → `${FOO_LIBRARIES}` | Link to imported target `Foo::Foo` |
| `FetchContent` with branch `main` | Pin to tag or commit SHA |
| `macro()` for utility logic | `function()` for proper scoping |
| Missing `PUBLIC`/`PRIVATE`/`INTERFACE` | Always explicit on all `target_*` calls |
