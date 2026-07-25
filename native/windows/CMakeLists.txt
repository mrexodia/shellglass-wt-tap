cmake_minimum_required(VERSION 3.25)
project(shellglass_native LANGUAGES CXX)

if(NOT WIN32 OR NOT MSVC)
  message(FATAL_ERROR "shellglass native render taps require MSVC on Windows")
endif()

function(shellglass_harden target)
  target_compile_features(${target} PRIVATE cxx_std_20)
  target_compile_definitions(${target} PRIVATE UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
  target_compile_options(${target} PRIVATE /W4 /WX /guard:cf)
  target_link_options(${target} PRIVATE /guard:cf)
endfunction()

add_executable(shellglass-native-mock mock_adapter.cpp)
shellglass_harden(shellglass-native-mock)

add_executable(shellglass-inject injector.cpp)
shellglass_harden(shellglass-inject)
set_property(TARGET shellglass-inject PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")

add_executable(shellglass-wt-fixture wt_fixture.cpp)
shellglass_harden(shellglass-wt-fixture)
set_property(TARGET shellglass-wt-fixture PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")

add_executable(shellglass-conpty-fixture WIN32 conpty_fixture.cpp)
shellglass_harden(shellglass-conpty-fixture)
set_property(TARGET shellglass-conpty-fixture PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")

add_executable(shellglass-conhost-client-fixture conhost_client_fixture.cpp)
shellglass_harden(shellglass-conhost-client-fixture)
set_property(TARGET shellglass-conhost-client-fixture PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")

if(NOT CMAKE_GENERATOR_PLATFORM STREQUAL "ARM64" AND NOT CMAKE_SYSTEM_PROCESSOR STREQUAL "ARM64")
  add_library(shellglass-wt-adapter SHARED wt_adapter.cpp)
  shellglass_harden(shellglass-wt-adapter)
  set_property(TARGET shellglass-wt-adapter PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
  target_link_libraries(shellglass-wt-adapter PRIVATE bcrypt ole32 windowscodecs)

  add_library(shellglass-wt-fault-adapter SHARED wt_adapter.cpp)
  shellglass_harden(shellglass-wt-fault-adapter)
  target_compile_definitions(shellglass-wt-fault-adapter PRIVATE SHELLGLASS_CALLBACK_FAULT_TEST=1)
  set_property(TARGET shellglass-wt-fault-adapter PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
  target_link_libraries(shellglass-wt-fault-adapter PRIVATE bcrypt ole32 windowscodecs)

  add_library(shellglass-conhost-adapter SHARED conhost_adapter.cpp)
  shellglass_harden(shellglass-conhost-adapter)
  set_property(TARGET shellglass-conhost-adapter PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
  target_link_libraries(shellglass-conhost-adapter PRIVATE bcrypt)

  add_library(shellglass-conhost-fault-adapter SHARED conhost_adapter.cpp)
  shellglass_harden(shellglass-conhost-fault-adapter)
  target_compile_definitions(shellglass-conhost-fault-adapter PRIVATE SHELLGLASS_CALLBACK_FAULT_TEST=1)
  set_property(TARGET shellglass-conhost-fault-adapter PROPERTY MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
  target_link_libraries(shellglass-conhost-fault-adapter PRIVATE bcrypt)
endif()

add_library(shellglass-profile-fixture SHARED profile_fixture.cpp)
shellglass_harden(shellglass-profile-fixture)
target_compile_options(shellglass-profile-fixture PRIVATE /Od /Zi)
target_link_options(shellglass-profile-fixture PRIVATE /OPT:NOREF /DEBUG:FULL)

add_library(shellglass-conhost-profile-fixture SHARED conhost_profile_fixture.cpp)
shellglass_harden(shellglass-conhost-profile-fixture)
target_compile_options(shellglass-conhost-profile-fixture PRIVATE /Od /Zi)
target_link_options(shellglass-conhost-profile-fixture PRIVATE /OPT:NOREF /DEBUG:FULL)

add_executable(shellglass-profile profile_tool.cpp)
shellglass_harden(shellglass-profile)
find_path(DIA_INCLUDE_DIR dia2.h
  HINTS "$ENV{VSINSTALLDIR}/DIA SDK/include" "${CMAKE_GENERATOR_INSTANCE}/DIA SDK/include"
  PATHS "C:/Program Files/Microsoft Visual Studio/2022/Community/DIA SDK/include")
if(CMAKE_GENERATOR_PLATFORM STREQUAL "ARM64" OR CMAKE_SYSTEM_PROCESSOR STREQUAL "ARM64")
  set(DIA_LIB_ARCH arm64)
else()
  set(DIA_LIB_ARCH amd64)
endif()
find_library(DIA_GUIDS_LIBRARY diaguids
  HINTS "$ENV{VSINSTALLDIR}/DIA SDK/lib/${DIA_LIB_ARCH}" "${CMAKE_GENERATOR_INSTANCE}/DIA SDK/lib/${DIA_LIB_ARCH}"
  PATHS "C:/Program Files/Microsoft Visual Studio/2022/Community/DIA SDK/lib/${DIA_LIB_ARCH}")
if(NOT DIA_INCLUDE_DIR OR NOT DIA_GUIDS_LIBRARY)
  message(FATAL_ERROR "Visual Studio DIA SDK is required for shellglass-profile")
endif()
target_include_directories(shellglass-profile PRIVATE "${DIA_INCLUDE_DIR}")
target_link_libraries(shellglass-profile PRIVATE bcrypt ole32 oleaut32 "${DIA_GUIDS_LIBRARY}")
