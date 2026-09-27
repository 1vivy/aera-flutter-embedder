#!/usr/bin/env bash
# Package the two kits that apps built from aera-flutter-template download:
#
#   aera-flutter-runtime-arm64-<flutter>.tar.xz  everything the app needs in
#       AERA's jail except the app: glibc, Flutter engine, Mesa, fonts and
#       this embedder (from tools/assemble_runtime.py)
#   aera-flutter-simkit-x64-<flutter>.tar.xz     the embedder, the host
#       simulator and the x64 engine, for running apps on a PC
#
# The engine is a debug (JIT) engine, so an app must be built with exactly the
# Flutter release named in the kit; each kit records it in `flutter-version`.
#
#   tools/package_kits.sh <runtime-dir> <x64-engine.so> <icudtl.dat> <out-dir>
set -euo pipefail
runtime=$1 engine_x64=$2 icu=$3 out=$4
flutter_version=$(flutter --version --machine | python3 -c 'import json,sys; print(json.load(sys.stdin)["frameworkVersion"])')
engine_hash=$(flutter --version --machine | python3 -c 'import json,sys; print(json.load(sys.stdin)["engineRevision"])')

mkdir -p "$out"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cp -a "$runtime" "$work/runtime"
printf '%s\n' "$flutter_version" > "$work/runtime/flutter-version"
printf '%s\n' "$engine_hash" > "$work/runtime/engine-revision"
tar -C "$work/runtime" -cJf "$out/aera-flutter-runtime-arm64-$flutter_version.tar.xz" .

cargo build --release --features sim
kit=$work/simkit
mkdir -p "$kit/bin" "$kit/usr/lib" "$kit/usr/share/flutter"
cp target/release/aera-browser-worker target/release/aera-host-sim "$kit/bin/"
cp "$engine_x64" "$kit/usr/lib/libflutter_engine.so"
cp "$icu" "$kit/usr/share/flutter/icudtl.dat"
printf '%s\n' "$flutter_version" > "$kit/flutter-version"
printf '%s\n' "$engine_hash" > "$kit/engine-revision"
tar -C "$kit" -cJf "$out/aera-flutter-simkit-x64-$flutter_version.tar.xz" .

ls -l "$out"
