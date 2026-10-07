# Lets FFmpeg be built with its libopenh264 H.264 encoder without building or
# linking OpenH264 itself: users download Cisco's OpenH264 binary separately
# (Thalamus fetches it into ~/.thalamus), which keeps it under Cisco's H.264
# patent license, and the plugin loads it at run time.
#
# FFmpeg compiles its wrapper against the OpenH264 2.6.0 API headers vendored
# in openh264/include (BSD licensed, see openh264/include/wels/LICENSE), and
# its configure check and command line tools link a stub library whose
# functions all fail. The plugin defines the encoder functions FFmpeg calls
# (rust/src/openh264.rs) and forwards them to Cisco's binary; the version it
# loads must match these headers.
#
# Sets OPENH264_STUB_TARGET and OPENH264_PKG_CONFIG_DIR.

set(OPENH264_STUB_DIR "${CMAKE_BINARY_DIR}/openh264-stub")
set(OPENH264_INCLUDE_DIR "${CMAKE_CURRENT_LIST_DIR}/openh264/include")
set(OPENH264_PKG_CONFIG_DIR "${OPENH264_STUB_DIR}/lib/pkgconfig")

add_library(openh264_stub STATIC "${CMAKE_CURRENT_LIST_DIR}/openh264/stub.c")
target_include_directories(openh264_stub PRIVATE "${OPENH264_INCLUDE_DIR}")
# openh264.lib / libopenh264.a, as configure's -lopenh264 expects.
set_target_properties(openh264_stub PROPERTIES
  OUTPUT_NAME openh264
  ARCHIVE_OUTPUT_DIRECTORY "${OPENH264_STUB_DIR}/lib")
set(OPENH264_STUB_TARGET openh264_stub)

file(WRITE "${OPENH264_PKG_CONFIG_DIR}/openh264.pc"
"prefix=${OPENH264_STUB_DIR}
libdir=\${prefix}/lib
includedir=${OPENH264_INCLUDE_DIR}

Name: OpenH264
Description: Stub for building FFmpeg's libopenh264 wrapper; the real library is loaded at run time
Version: 2.6.0
Libs: -L\${libdir} -lopenh264
Cflags: -I\${includedir}
")
