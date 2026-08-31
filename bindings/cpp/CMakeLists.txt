# kevy C++ client — CMake build.
#
#   cargo build -p kevy-ffi          # the embedded C ABI staticlib (linked in)
#   cargo build --release -p kevy    # the server (spawned by remote tests)
#   cmake -S bindings/cpp -B bindings/cpp/build && cmake --build bindings/cpp/build
#   ctest --test-dir bindings/cpp/build --output-on-failure
#
# Layout:
#   include/kevy/*   public headers (the CLIENT layer)
#   src/*.cpp        the client implementation
#   test/*           a dependency-free test harness + conformance suite (§6)
cmake_minimum_required(VERSION 3.16)
project(kevy_client VERSION 6.2.1 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Debug)
endif()

# Repo root, relative to bindings/cpp.
set(KEVY_REPO_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.." CACHE PATH "kevy repo root")
set(KEVY_FFI_INCLUDE "${KEVY_REPO_ROOT}/crates/kevy-ffi/include" CACHE PATH "kevy-ffi C ABI headers")

# The prebuilt embedded C ABI staticlib (cargo build -p kevy-ffi).
find_library(KEVY_FFI_LIB
  NAMES kevy_ffi libkevy_ffi.a
  PATHS "${KEVY_REPO_ROOT}/target/debug" "${KEVY_REPO_ROOT}/target/release"
  NO_DEFAULT_PATH)
if(NOT KEVY_FFI_LIB)
  message(FATAL_ERROR "libkevy_ffi.a not found — run: cargo build -p kevy-ffi")
endif()

find_package(Threads REQUIRED)

file(GLOB KEVY_CLIENT_SRC CONFIGURE_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/src/*.cpp")
add_library(kevy_client STATIC ${KEVY_CLIENT_SRC})
target_include_directories(kevy_client
  PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}/include"
  PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/src" "${KEVY_FFI_INCLUDE}")
target_link_libraries(kevy_client PUBLIC "${KEVY_FFI_LIB}" Threads::Threads)
target_compile_options(kevy_client PRIVATE -Wall -Wextra)
# macOS: the Rust staticlib may reference these system libs.
if(APPLE)
  target_link_libraries(kevy_client PUBLIC "-framework CoreFoundation" "-framework Security")
endif()

# --- tests ---------------------------------------------------------------
enable_testing()
set(KEVY_SERVER_BIN "${KEVY_REPO_ROOT}/target/release/kevy" CACHE FILEPATH "kevy server binary")

file(GLOB KEVY_TEST_SRC CONFIGURE_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/test/*.cpp")
add_executable(kevy_tests ${KEVY_TEST_SRC})
target_include_directories(kevy_tests PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/test")
target_link_libraries(kevy_tests PRIVATE kevy_client)
target_compile_definitions(kevy_tests PRIVATE KEVY_SERVER_BIN="${KEVY_SERVER_BIN}")
target_compile_options(kevy_tests PRIVATE -Wall -Wextra)

add_test(NAME kevy_conformance COMMAND kevy_tests)
