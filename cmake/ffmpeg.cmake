FetchContent_Declare(
  ffmpeg 
  GIT_REPOSITORY https://github.com/FFmpeg/FFmpeg.git
  GIT_TAG        n7.1.5
)
FetchContent_MakeAvailable(ffmpeg)
file(MAKE_DIRECTORY "${ffmpeg_BINARY_DIR}/Debug/Modules")
file(MAKE_DIRECTORY "${ffmpeg_BINARY_DIR}/Release/Modules")

if(APPLE)
  list(APPEND FFMPEG_EXTRA_FLAGS --disable-libxcb)
endif()
if(SANITIZER)
  list(APPEND FFMPEG_EXTRA_FLAGS --disable-asm)
endif()
if(WIN32)
  list(APPEND FFMPEG_EXTRA_FLAGS --target-os=win64 --arch=x86_64)
else()
  list(APPEND FFMPEG_EXTRA_FLAGS --disable-sndio --enable-pic --enable-openssl)
endif()

if(WIN32)
  string(REGEX REPLACE "^([a-zA-Z]):" "/\\1" FFMPEG_COMPILER "${CMAKE_C_COMPILER}")
  string(REPLACE "clang-cl" "clang" FFMPEG_COMPILER "${FFMPEG_COMPILER}")
  string(REPLACE "Program Files (x86)" "Progra~2" FFMPEG_COMPILER "${FFMPEG_COMPILER}")
  string(REPLACE "Program Files" "Progra~1" FFMPEG_COMPILER "${FFMPEG_COMPILER}")

  string(REGEX REPLACE "^([a-zA-Z]):" "/\\1" FFMPEG_CXX_COMPILER "${CMAKE_CXX_COMPILER}")
  string(REPLACE "clang-cl" "clang" FFMPEG_CXX_COMPILER "${FFMPEG_CXX_COMPILER}")
  string(REPLACE "Program Files (x86)" "Progra~2" FFMPEG_CXX_COMPILER "${FFMPEG_CXX_COMPILER}")
  string(REPLACE "Program Files" "Progra~1" FFMPEG_CXX_COMPILER "${FFMPEG_CXX_COMPILER}")
else()
  set(FFMPEG_COMPILER "${CMAKE_C_COMPILER}")
  set(FFMPEG_CXX_COMPILER "${CMAKE_CXX_COMPILER}")
endif()
message("FFMPEG_COMPILER ${FFMPEG_COMPILER}")
message("FFMPEG_CXX_COMPILER ${FFMPEG_CXX_COMPILER}")

if(WIN32)
  # Plain clang (as opposed to clang-cl) defaults to the static MSVC CRT
  # (libcmt/libcpmt) on the windows-msvc target, unlike CMake's own default
  # (dynamic, /MD) that the rest of this project now relies on and that
  # skia-bindings' prebuilt skia.lib and Rust's own windows-msvc target
  # already use. Force ffmpeg onto the dynamic CRT too, or the final link
  # fails with RuntimeLibrary mismatches (LNK2038/LNK2005). This has to go
  # on extra-ldflags as well as extra-cflags/-cxxflags: configure's own
  # link-check step (test_ld) invokes the compiler driver again without
  # extra-cflags, so without it here that check step links against the
  # default (static) CRT and fails to resolve the dynamic-CRT imports
  # (e.g. __imp__aligned_malloc) the extra-cflags compile step emitted.
  set(FFMPEG_EXTRA_CFLAGS "${ALL_C_COMPILE_OPTIONS_SPACED} -fms-runtime-lib=dll")
  set(FFMPEG_EXTRA_CXXFLAGS "-fms-runtime-lib=dll")
  # -fms-runtime-lib=dll alone isn't enough at link time either: plain
  # clang (as opposed to clang-cl) always hands link.exe "-defaultlib:libcmt
  # -defaultlib:oldnames" regardless of -fms-runtime-lib, which still
  # conflicts with the msvcrt-linked object files. Override explicitly.
  set(FFMPEG_EXTRA_LDFLAGS "${ALL_C_LINK_OPTIONS_SPACED} -fms-runtime-lib=dll -Xlinker -defaultlib:msvcrt -Xlinker -nodefaultlib:libcmt -Xlinker -defaultlib:msvcprt -Xlinker -nodefaultlib:libcpmt")
else()
  set(FFMPEG_EXTRA_CFLAGS "${ALL_C_COMPILE_OPTIONS_SPACED}")
  set(FFMPEG_EXTRA_CXXFLAGS "")
  set(FFMPEG_EXTRA_LDFLAGS "${ALL_C_LINK_OPTIONS_SPACED}")
endif()

add_library(ffmpeg_m m_stub.cpp)
set_target_properties(ffmpeg_m PROPERTIES 
  ARCHIVE_OUTPUT_DIRECTORY "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>"
  LIBRARY_OUTPUT_DIRECTORY "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>"
  OUTPUT_NAME m)

if(WIN32)
  set(FFMPEG_POST_CONFIG && sed -i s/LIBPREF=lib/LIBPREF=/ ffbuild/config.mak
                         && sed -i s/LIBSUF=.a/LIBSUF=.lib/ ffbuild/config.mak)
endif()

add_custom_command(
  OUTPUT "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/Makefile"
  DEPENDS ffmpeg_m
  COMMAND cmake -E env 
  "PKG_CONFIG_PATH=${ZLIB_PKG_CONFIG_DIR}:${SDL_PKG_CONFIG_DIR}"
  sh "${ffmpeg_SOURCE_DIR}/configure" ${FFMPEG_EXTRA_FLAGS} "--cc=${FFMPEG_COMPILER}" "--cxx=${FFMPEG_CXX_COMPILER}" "--extra-cflags=${FFMPEG_EXTRA_CFLAGS}" "--extra-cxxflags=${FFMPEG_EXTRA_CXXFLAGS}" "--extra-ldflags=${FFMPEG_EXTRA_LDFLAGS}" --enable-static --disable-shared --disable-sndio $<IF:$<CONFIG:Debug>,--enable-debug,> --prefix=${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install
  && cmake -E touch_nocreate "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/Makefile"
  ${FFMPEG_POST_CONFIG}
  WORKING_DIRECTORY "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>")

if(WIN32)
  set(FFMPEG_OUTPUT_LIBRARIES
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/avcodec.lib"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/avdevice.lib"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/avfilter.lib"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/avformat.lib"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/avutil.lib"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/swresample.lib"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/swscale.lib")
  set(FFMPEG_LIBRARIES "${FFMPEG_OUTPUT_LIBRARIES}"
    Ws2_32.lib Secur32.lib Bcrypt.lib Mfplat.lib Ole32.lib User32.lib dxguid.lib uuid.lib Mfuuid.lib strmiids.lib Kernel32.lib Psapi.lib)
else()
  set(FFMPEG_OUTPUT_LIBRARIES
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/libavcodec.a"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/libavdevice.a"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/libavfilter.a"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/libavformat.a"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/libavutil.a"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/libswresample.a"
    "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/lib/libswscale.a")
  set(FFMPEG_LIBRARIES "${FFMPEG_OUTPUT_LIBRARIES}")
endif()

list(GET FFMPEG_LIBRARIES 0 FIRST_FFMPEG_LIB)

set(FFMPEG_EXECUTABLE "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/bin/ffmpeg${CMAKE_EXECUTABLE_SUFFIX}")
set(FFPLAY_EXECUTABLE "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/bin/ffplay${CMAKE_EXECUTABLE_SUFFIX}")
set(FFPROBE_EXECUTABLE "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/bin/ffprobe${CMAKE_EXECUTABLE_SUFFIX}")

add_custom_command(
  OUTPUT "${FFMPEG_OUTPUT_LIBRARIES}" ${FFTOOL_OBJECTS} "${ffmpeg_BINARY_DIR}/$<CONFIG>/ffbuild/config.mak" 
  DEPENDS "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/Makefile"
  COMMAND make -j ${CPU_COUNT} install
  && cmake -E touch_nocreate ${FIRST_FFMPEG_LIB}
  WORKING_DIRECTORY "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>")

set(FFMPEG_FOUND 1)
set(FFMPEG_INCLUDE_DIRS
  "${ffmpeg_BINARY_DIR}/$<IF:$<CONFIG:Debug>,Debug,Release>/install/include")
string(REPLACE "$<IF:$<CONFIG:Debug>,Debug,Release>" "Debug" FFMPEG_INCLUDE_DIRS_DEBUG "${FFMPEG_INCLUDE_DIRS}")
string(REPLACE "$<IF:$<CONFIG:Debug>,Debug,Release>" "Release" FFMPEG_INCLUDE_DIRS_RELEASE "${FFMPEG_INCLUDE_DIRS}")
string(REPLACE "$<IF:$<CONFIG:Debug>,Debug,Release>" "Debug" FFMPEG_LIBRARIES_DEBUG "${FFMPEG_LIBRARIES}")
string(REPLACE "$<IF:$<CONFIG:Debug>,Debug,Release>" "Release" FFMPEG_LIBRARIES_RELEASE "${FFMPEG_LIBRARIES}")

set(FIND_FFMPEG "")
set(FIND_FFMPEG "${FIND_FFMPEG}execute_process(COMMAND ${ffmpeg_BINARY_DIR}/Debug/install/bin/ffmpeg -version\n")
set(FIND_FFMPEG "${FIND_FFMPEG}                OUTPUT_VARIABLE FFMPEG_OUTPUT)\n")
set(FIND_FFMPEG "${FIND_FFMPEG}string(REGEX MATCHALL \"lib[^/]+\" VERSIONS \"\${FFMPEG_OUTPUT}\")\n")
set(FIND_FFMPEG "${FIND_FFMPEG}foreach(line \${VERSIONS})\n")
set(FIND_FFMPEG "${FIND_FFMPEG}  string(REGEX REPLACE \"[a-z ]\" \"\" version \${line})\n")
set(FIND_FFMPEG "${FIND_FFMPEG}  string(REGEX REPLACE \"[ .0-9]\" \"\" lib \${line})\n")
set(FIND_FFMPEG "${FIND_FFMPEG}  set(FFMPEG_\${lib}_VERSION \${version})\n")
set(FIND_FFMPEG "${FIND_FFMPEG}  message(\"FFMPEG_\${lib}_VERSION \${FFMPEG_\${lib}_VERSION}\")\n")
set(FIND_FFMPEG "${FIND_FFMPEG}endforeach()\n")
set(FIND_FFMPEG "${FIND_FFMPEG}set(FFMPEG_FOUND 1)\n")
set(FIND_FFMPEG "${FIND_FFMPEG}set(FFMPEG_INCLUDE_DIRS \"${FFMPEG_INCLUDE_DIRS_DEBUG}\")\n")
set(FIND_FFMPEG "${FIND_FFMPEG}set(FFMPEG_LIBRARIES \"${FFMPEG_LIBRARIES_DEBUG}\")\n")
file(WRITE "${ffmpeg_BINARY_DIR}/Debug/Modules/FindFFMPEG.cmake" "${FIND_FFMPEG}")
set(FIND_FFMPEG "")
set(FIND_FFMPEG "${FIND_FFMPEG}execute_process(COMMAND ${ffmpeg_BINARY_DIR}/Release/install/bin/ffmpeg -version\n")
set(FIND_FFMPEG "${FIND_FFMPEG}                OUTPUT_VARIABLE FFMPEG_OUTPUT)\n")
set(FIND_FFMPEG "${FIND_FFMPEG}string(REGEX MATCHALL \"lib[^/]+\" VERSIONS \"\${FFMPEG_OUTPUT}\")\n")
set(FIND_FFMPEG "${FIND_FFMPEG}foreach(line \${VERSIONS})\n")
set(FIND_FFMPEG "${FIND_FFMPEG}  string(REGEX REPLACE \"[a-z ]\" \"\" version \${line})\n")
set(FIND_FFMPEG "${FIND_FFMPEG}  string(REGEX REPLACE \"[ .0-9]\" \"\" lib \${line})\n")
set(FIND_FFMPEG "${FIND_FFMPEG}  set(FFMPEG_\${lib}_VERSION \${version})\n")
set(FIND_FFMPEG "${FIND_FFMPEG}  message(\"FFMPEG_\${lib}_VERSION \${FFMPEG_\${lib}_VERSION}\")\n")
set(FIND_FFMPEG "${FIND_FFMPEG}endforeach()\n")
set(FIND_FFMPEG "${FIND_FFMPEG}set(FFMPEG_FOUND 1)\n")
set(FIND_FFMPEG "${FIND_FFMPEG}set(FFMPEG_INCLUDE_DIRS \"${FFMPEG_INCLUDE_DIRS_RELEASE}\")\n")
set(FIND_FFMPEG "${FIND_FFMPEG}set(FFMPEG_LIBRARIES \"${FFMPEG_LIBRARIES_RELEASE}\")\n")
file(WRITE "${ffmpeg_BINARY_DIR}/Release/Modules/FindFFMPEG.cmake" "${FIND_FFMPEG}")

add_library(ffmpeg INTERFACE "${FIRST_FFMPEG_LIB}")
target_include_directories(ffmpeg INTERFACE ${FFMPEG_INCLUDE_DIRS})
target_link_libraries(ffmpeg INTERFACE ${FFMPEG_LIBRARIES})
if(WIN32)
elseif(APPLE)
  target_link_libraries(ffmpeg INTERFACE bz2)
else()
  target_link_libraries(ffmpeg INTERFACE va X11 lzma va-drm va-x11 vdpau Xext Xv asound bz2)
endif()
