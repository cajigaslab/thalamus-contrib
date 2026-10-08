# libvpx (BSD), for FFmpeg's libvpx-vp9 VP9 encoder: FFmpeg has no software
# VP9 encoder of its own. Only the VP9 encoder is built; decoding uses FFmpeg's
# own VP9 decoder. Included by ffmpeg.cmake when it builds FFmpeg from source.
#
# Built with libvpx's configure and make on every platform, as a static library,
# in ${libvpx_BINARY_DIR}/${CMAKE_BUILD_TYPE} and installed, with vpx.pc, into
# ${libvpx_BINARY_DIR}/${CMAKE_BUILD_TYPE}/install. Debug builds are configured
# with --enable-debug, as FFmpeg's are.
# On Windows its mingw-style target is built with clang, which produces MSVC
# compatible objects (as FFmpeg's build does), against the dynamic CRT. Its x86
# assembly needs nasm. Sets VPX_LIBRARY and VPX_PKG_CONFIG_DIR.

FetchContent_Declare(
  libvpx
  GIT_REPOSITORY https://chromium.googlesource.com/webm/libvpx
  GIT_TAG        v1.17.0
)
FetchContent_MakeAvailable(libvpx)
set(VPX_BUILD_DIR "${libvpx_BINARY_DIR}/${CMAKE_BUILD_TYPE}")
set(VPX_INSTALL_DIR "${VPX_BUILD_DIR}/install")
set(VPX_PKG_CONFIG_DIR "${VPX_INSTALL_DIR}/lib/pkgconfig")
file(MAKE_DIRECTORY "${VPX_BUILD_DIR}")

set(VPX_CONFIGURE_FLAGS
  "--prefix=${VPX_INSTALL_DIR}"
  --disable-examples --disable-tools --disable-docs --disable-unit-tests
  --disable-vp8 --disable-vp9-decoder --enable-vp9)
if(CMAKE_BUILD_TYPE STREQUAL "Debug")
  list(APPEND VPX_CONFIGURE_FLAGS --enable-debug)
endif()

if(CMAKE_SYSTEM_PROCESSOR MATCHES "^(x86_64|AMD64|amd64|i.86|x86)$")
  find_program(NASM_EXECUTABLE nasm)
  if(NOT NASM_EXECUTABLE)
    message(FATAL_ERROR "libvpx (VP9 encoding) needs nasm on x86")
  endif()
  list(APPEND VPX_CONFIGURE_FLAGS --as=nasm)
endif()

if(WIN32)
  set(VPX_ENV CC=clang CXX=clang++ AR=llvm-ar LD=clang++ STRIP=llvm-strip AS=nasm)
  list(APPEND VPX_CONFIGURE_FLAGS --target=x86_64-win64-gcc
    --extra-cflags=-fms-runtime-lib=dll --extra-cxxflags=-fms-runtime-lib=dll)
  set(VPX_INSTALLED "${VPX_INSTALL_DIR}/lib/libvpx.a")
  set(VPX_LIBRARY "${VPX_INSTALL_DIR}/lib/vpx.lib")
  set(VPX_POST_INSTALL
    # clang defines _MSC_VER, so libvpx's headers call the MSVC-only x87
    # control word helpers, which only its Visual Studio builds assemble.
    COMMAND nasm -f win64 "-I${libvpx_SOURCE_DIR}/" -I./ -o float_control_word.obj
      "${libvpx_SOURCE_DIR}/vpx_ports/float_control_word.asm"
    COMMAND llvm-ar rs "${VPX_INSTALLED}" float_control_word.obj
    # make install writes libvpx.a; FFmpeg's -lvpx looks for vpx.lib.
    COMMAND "${CMAKE_COMMAND}" -E copy "${VPX_INSTALLED}" "${VPX_LIBRARY}")
else()
  set(VPX_ENV "CC=${CMAKE_C_COMPILER}" "CXX=${CMAKE_CXX_COMPILER}")
  list(APPEND VPX_CONFIGURE_FLAGS --enable-pic)
  if(APPLE)
    list(APPEND VPX_ENV "MACOSX_DEPLOYMENT_TARGET=${CMAKE_OSX_DEPLOYMENT_TARGET}")
    list(APPEND VPX_CONFIGURE_FLAGS "--extra-cflags=-mmacosx-version-min=${CMAKE_OSX_DEPLOYMENT_TARGET}"
      "--extra-cxxflags=-mmacosx-version-min=${CMAKE_OSX_DEPLOYMENT_TARGET}")
  endif()
  set(VPX_LIBRARY "${VPX_INSTALL_DIR}/lib/libvpx.a")
  set(VPX_POST_INSTALL)
endif()

add_custom_command(
  OUTPUT "${VPX_LIBRARY}"
  COMMAND "${CMAKE_COMMAND}" -E env ${VPX_ENV}
    sh "${libvpx_SOURCE_DIR}/configure" ${VPX_CONFIGURE_FLAGS}
  COMMAND make -j ${CPU_COUNT}
  COMMAND make install
  ${VPX_POST_INSTALL}
  WORKING_DIRECTORY "${VPX_BUILD_DIR}"
  VERBATIM)
