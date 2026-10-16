#!/usr/bin/env bash
# ort no longer publishes Intel Mac prebuilts. Build its API-24 runtime natively.
set -euo pipefail
[[ "$(uname -s)" == Darwin && "$(uname -m)" == x86_64 ]]
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
: "${GITHUB_ENV:?GITHUB_ENV is required}"
runtime_dir="$RUNNER_TEMP/solo-onnxruntime-runtime"
if [[ ! -f "$runtime_dir/libonnxruntime.dylib" ]]; then
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
  mkdir -p "$runtime_dir"
  cp -L "$build_dir/Release"/*.dylib "$runtime_dir/"
  cp "$source_dir/LICENSE" "$runtime_dir/onnxruntime-LICENSE.txt"
  cp "$source_dir/ThirdPartyNotices.txt" "$runtime_dir/onnxruntime-NOTICES.txt"
fi
test -f "$runtime_dir/libonnxruntime.dylib"
test -s "$runtime_dir/onnxruntime-LICENSE.txt"
test -s "$runtime_dir/onnxruntime-NOTICES.txt"
# lipo treats every argument after -verify_arch as an architecture, so input first.
lipo "$runtime_dir/libonnxruntime.dylib" -verify_arch x86_64
# ort-sys accepts this library directory for compile-time dynamic linking.
echo "ORT_LIB_LOCATION=$runtime_dir" >> "$GITHUB_ENV"
echo "ORT_PREFER_DYNAMIC_LINK=1" >> "$GITHUB_ENV"
