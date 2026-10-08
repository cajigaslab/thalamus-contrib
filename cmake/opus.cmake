# libopus (BSD), for FFmpeg's libopus Opus encoder; FFmpeg's own Opus encoder
# is experimental. Included by ffmpeg.cmake when it builds FFmpeg from source.
#
# Built with its own CMake build as a static library of the same build type as
# this project in ${opus_BINARY_DIR}/${CMAKE_BUILD_TYPE} and installed, with
# opus.pc, into ${opus_BINARY_DIR}/${CMAKE_BUILD_TYPE}/install. Sets
# OPUS_LIBRARY and OPUS_PKG_CONFIG_DIR.

FetchContent_Declare(
  opus
  GIT_REPOSITORY https://github.com/xiph/opus.git
  GIT_TAG        v1.5.2
  # Only fetched here: it's built as a separate project below, not added to
  # this one.
  SOURCE_SUBDIR  not-a-cmake-subdirectory
)
FetchContent_MakeAvailable(opus)

set(OPUS_BUILD_DIR "${opus_BINARY_DIR}/${CMAKE_BUILD_TYPE}")
set(OPUS_INSTALL_DIR "${OPUS_BUILD_DIR}/install")
set(OPUS_PKG_CONFIG_DIR "${OPUS_INSTALL_DIR}/lib/pkgconfig")
if(WIN32)
  set(OPUS_LIBRARY "${OPUS_INSTALL_DIR}/lib/opus.lib")
else()
  set(OPUS_LIBRARY "${OPUS_INSTALL_DIR}/lib/libopus.a")
endif()

add_custom_command(
  OUTPUT "${OPUS_LIBRARY}"
  COMMAND "${CMAKE_COMMAND}" -S "${opus_SOURCE_DIR}" -B "${OPUS_BUILD_DIR}"
    -G "${CMAKE_GENERATOR}" "-DCMAKE_MAKE_PROGRAM=${CMAKE_MAKE_PROGRAM}"
    "-DCMAKE_BUILD_TYPE=${CMAKE_BUILD_TYPE}"
    "-DCMAKE_C_COMPILER=${CMAKE_C_COMPILER}"
    "-DCMAKE_OSX_DEPLOYMENT_TARGET=${CMAKE_OSX_DEPLOYMENT_TARGET}"
    # The dynamic release CRT on Windows, like the rest of this build (and
    # Rust's windows-msvc target), even in a Debug build.
    -DCMAKE_POLICY_DEFAULT_CMP0091=NEW -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON
    -DBUILD_SHARED_LIBS=OFF -DOPUS_BUILD_PROGRAMS=OFF -DOPUS_BUILD_TESTING=OFF
    -DOPUS_INSTALL_PKG_CONFIG_MODULE=ON
    -DCMAKE_INSTALL_LIBDIR=lib "-DCMAKE_INSTALL_PREFIX=${OPUS_INSTALL_DIR}"
  COMMAND "${CMAKE_COMMAND}" --build "${OPUS_BUILD_DIR}"
  COMMAND "${CMAKE_COMMAND}" --install "${OPUS_BUILD_DIR}"
  VERBATIM)
