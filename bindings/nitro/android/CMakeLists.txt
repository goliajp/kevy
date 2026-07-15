project(KevyNitro)
cmake_minimum_required(VERSION 3.9.0)

set(PACKAGE_NAME KevyNitro)
set(CMAKE_VERBOSE_MAKEFILE ON)
set(CMAKE_CXX_STANDARD 20)

# The Nitro C++ HybridObject + the fbjni OnLoad adapter.
add_library(${PACKAGE_NAME} SHARED
        src/main/cpp/cpp-adapter.cpp
        ../cpp/HybridKevyNitro.cpp
)

# Nitrogen-generated specs + autolinking registration.
include(${CMAKE_SOURCE_DIR}/../nitrogen/generated/android/KevyNitro+autolinking.cmake)

# Local includes: cpp-adapter dir and our HybridKevyNitro.hpp / kevy.h.
include_directories(
        "src/main/cpp"
        "../cpp"
)

# The prebuilt kevy engine C ABI (kevy-ffi), vendored per-ABI under
# jniLibs. Import it and link — the .so is packaged into the APK by AGP,
# so the dynamic loader finds it at runtime.
add_library(kevy_ffi SHARED IMPORTED)
set_target_properties(kevy_ffi PROPERTIES
        IMPORTED_LOCATION "${CMAKE_SOURCE_DIR}/src/main/jniLibs/${ANDROID_ABI}/libkevy_ffi.so"
)

find_library(LOG_LIB log)

target_link_libraries(
        ${PACKAGE_NAME}
        ${LOG_LIB}
        android
        kevy_ffi
)
