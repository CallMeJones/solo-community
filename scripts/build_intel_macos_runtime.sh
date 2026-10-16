#!/usr/bin/env bash
# ort no longer publishes Intel Mac prebuilts. Build its API-24 runtime natively.
set -euo pipefail
[[ "$(uname -s)" == Darwin && "$(uname -m)" == x86_64 ]]
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
: "${GITHUB_ENV:?GITHUB_ENV is required}"
source_dir="$RUNNER_TEMP/solo-onnxruntime-source"
build_dir="$RUNNER_TEMP/solo-onnxruntime-build"
# Microsoft ONNX Runtime v1.24.4. Keep source identity immutable.
commit=2d924974ef147392ced8409d36bd6d2e7fcc8a74
git init "$source_dir"
git -C "$source_dir" remote add origin https://github.com/microsoft/onnxruntime.git
git -C "$source_dir" fetch --depth 1 origin "$commit"
git -C "$source_dir" checkout --detach FETCH_HEAD
[[ "$(git -C "$source_dir" rev-parse HEAD)" == "$commit" ]]
git -C "$source_dir" submodule update --init --recursive
bash "$source_dir/build.sh" --config Release --build_shared_lib \
  --parallel 3 --skip_tests --compile_no_warning_as_error \
  --build_dir "$build_dir" --cmake_extra_defines \
  CMAKE_OSX_ARCHITECTURES=x86_64 CMAKE_OSX_DEPLOYMENT_TARGET=15.0 \
  onnxruntime_BUILD_UNIT_TESTS=OFF
library_dir="$build_dir/Release"
test -f "$library_dir/libonnxruntime.dylib"
lipo -verify_arch x86_64 "$library_dir/libonnxruntime.dylib"
# ort-sys accepts this library directory for compile-time dynamic linking.
echo "ORT_LIB_LOCATION=$library_dir" >> "$GITHUB_ENV"
echo "ORT_PREFER_DYNAMIC_LINK=1" >> "$GITHUB_ENV"
echo "SOLO_ONNX_SOURCE=$source_dir" >> "$GITHUB_ENV"
