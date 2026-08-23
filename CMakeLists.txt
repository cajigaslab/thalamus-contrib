cmake_minimum_required(VERSION 3.16)
cmake_policy(SET CMP0042 NEW)
cmake_policy(SET CMP0071 NEW)
cmake_policy(SET CMP0077 NEW)
cmake_policy(SET CMP0091 NEW)
if(${CMAKE_VERSION} VERSION_GREATER_EQUAL "3.24")
  cmake_policy(SET CMP0135 NEW)
endif()
set(CMAKE_POLICY_DEFAULT_CMP0042 NEW)
set(CMAKE_POLICY_DEFAULT_CMP0063 NEW)
set(CMAKE_POLICY_DEFAULT_CMP0077 NEW)

project(thalamus)

option(CODE_COVERAGE "Enable Code Coverage" OFF)
option(STRIP "Strip native execute when copied to thalamus dir" OFF)

set(THALAMUS_PYTHON "${PYTHON}")

message("CMAKE_SYSTEM_PROCESSOR ${CMAKE_SYSTEM_PROCESSOR}")

if(APPLE)
  set(OSX_TARGET_PARAMETER -mmacosx-version-min=${CMAKE_OSX_DEPLOYMENT_TARGET})
  list(APPEND ALL_COMPILE_OPTIONS ${OSX_TARGET_PARAMETER})
endif()

# Default to C99
if(NOT CMAKE_C_STANDARD) 
  set(CMAKE_C_STANDARD 99)
endif()

# Default to C++17
set(CMAKE_CXX_STANDARD 20)

set(ABSL_PROPAGATE_CXX_STD ON)

include(FetchContent)

include(ProcessorCount)
ProcessorCount(CPU_COUNT)
if(CPU_COUNT EQUAL 0)
  set(MAKE_J)
  set(CMAKE_PARALLEL)
else()
  if(WIN32)
    set(MAKE_J /J ${CPU_COUNT})
  else()
    set(MAKE_J -j ${CPU_COUNT})
  endif()
  set(CMAKE_PARALLEL --parallel ${CPU_COUNT})
endif()

message("Release debug symbols enabled")
list(APPEND ALL_COMPILE_OPTIONS -g)
  
if("${SANITIZER}" STREQUAL thread)
  list(APPEND ALL_COMPILE_OPTIONS -fsanitize=thread -O1)
  list(APPEND ALL_LINK_OPTIONS -fsanitize=thread -O1)
elseif("${SANITIZER}" STREQUAL address)
  if("${CMAKE_CXX_COMPILER_ID}" MATCHES "MSVC")
    list(APPEND ALL_COMPILE_OPTIONS -fsanitize=address,undefined)
  else()
    list(APPEND ALL_COMPILE_OPTIONS -fsanitize=address,undefined -O1 -fno-omit-frame-pointer -fno-optimize-sibling-calls)
    list(APPEND ALL_LINK_OPTIONS -fsanitize=address,undefined -O1 -fno-omit-frame-pointer -fno-optimize-sibling-calls)
  endif()
elseif("${SANITIZER}" STREQUAL memory)
  list(APPEND ALL_COMPILE_OPTIONS -fsanitize=memory -fsanitize-memory-track-origins=2 -O1 -fno-omit-frame-pointer -fno-optimize-sibling-calls)
  list(APPEND ALL_LINK_OPTIONS -fsanitize=memory -fsanitize-memory-track-origins=2 -O1 -fno-omit-frame-pointer -fno-optimize-sibling-calls)
endif()

string(REPLACE ";" " " ALL_C_COMPILE_OPTIONS_SPACED "${ALL_COMPILE_OPTIONS}")
string(REPLACE ";" " " ALL_C_LINK_OPTIONS_SPACED "${ALL_LINK_OPTIONS}")
 
add_compile_options("$<$<COMPILE_LANGUAGE:CXX>:${ALL_COMPILE_OPTIONS}>")
add_link_options("$<$<COMPILE_LANGUAGE:CXX>:${ALL_LINK_OPTIONS}>")
string(REPLACE ";" " " ALL_COMPILE_OPTIONS_SPACED "${ALL_COMPILE_OPTIONS}")
string(REPLACE ";" " " ALL_LINK_OPTIONS_SPACED "${ALL_LINK_OPTIONS}")

add_definitions(-DPRINTSYNC)

include(cmake/ffmpeg.cmake)

