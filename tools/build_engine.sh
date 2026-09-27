#!/usr/bin/env bash
# Build Flutter's embedder engine for arm64 Linux from source, in release or
# profile (AOT) mode, together with the x64-hosted gen_snapshot that compiles
# apps for it. Google publishes only a debug (JIT) arm64 embedder engine.
#
#   tools/build_engine.sh <flutter-version> <release|profile> <out-dir>
#
# Writes <out-dir>/aera-flutter-engine-arm64-<mode>-<flutter-version>.tar.xz:
#   usr/lib/libflutter_engine.so    the engine, stripped
#   usr/share/flutter/icudtl.dat
#   include/flutter_embedder.h
#   host/linux-x64/gen_snapshot     compiles app.dill into an arm64 libapp.so
#   flutter-version, engine-revision, runtime-mode, gn-args
# and the unstripped engine as ...-symbols.tar.xz.
#
# The engine is a plain arm64 glibc Linux embedder engine and knows nothing
# about AERA; the embedder picks the renderer: OpenGL (EGL) or Vulkan, with
# Skia or Impeller.
#
# Needs git, python3, curl, xz, about 50 GB of disk, and network access to
# github.com, *.googlesource.com, chrome-infra-packages.appspot.com and
# storage.googleapis.com. ENGINE_WORK_DIR (default ./.engine-build) keeps the
# checkout between runs.
set -euo pipefail

version=$1 mode=$2 out=$(realpath -m "$3")
case $mode in release | profile) ;; *) echo "mode must be release or profile" >&2; exit 2 ;; esac
work=$(realpath -m "${ENGINE_WORK_DIR:-.engine-build}")
mkdir -p "$work" "$out"
cd "$work"

# Not shallow: git 2.55 fails shallow clones of googlesource repos with
# "update_ref failed ... nonexistent object".
[ -d depot_tools ] || git clone https://chromium.googlesource.com/chromium/tools/depot_tools.git
export PATH=$work/depot_tools:$PATH

# The engine lives in the flutter/flutter monorepo; the release tag pins the
# framework and engine together, so the Dart VM in the engine matches the
# frontend_server shipped with that Flutter SDK.
if [ -d flutter ]; then
    # A kept work dir may hold another release; switch it to this one.
    git -C flutter fetch --depth 1 origin "refs/tags/$version:refs/tags/$version"
    git -C flutter checkout -q --force --detach "refs/tags/$version"
else
    git clone --depth 1 --branch "$version" https://github.com/flutter/flutter.git
fi
cd flutter
# Same revision string the kits and `flutter --version` report.
rev=$(cat bin/internal/engine.version 2>/dev/null || git rev-parse HEAD)

# engine/scripts/standard.gclient, minus deps an arm64 Linux engine never uses.
cat > .gclient <<'EOF'
solutions = [{
  "name": ".",
  "url": "https://github.com/flutter/flutter.git",
  "deps_file": "DEPS",
  "managed": False,
  "custom_deps": {},
  "custom_vars": {
    "download_android_deps": False,
    "download_fuchsia_deps": False,
    "download_jdk": False,
    "download_esbuild": False,
    "download_emsdk": False,
  },
}]
EOF
# Shallow deps save ~10 GB; fall back to full history if this git trips over
# shallow fetches the way it does on the depot_tools clone.
gclient sync --no-history --shallow -D || {
    echo "Shallow sync failed; retrying with history" >&2
    gclient sync -D
}

cd engine/src
target=aera_${mode}_arm64
gn_args=(
    --target-os linux --linux-cpu arm64 --runtime-mode "$mode"
    --target-dir "$target"
    --no-rbe --no-goma
    # Same as Google's linux_*_arm64 builders; LTO would double link memory.
    --no-lto --prebuilt-dart-sdk
    # Only the embedder API: no GTK shell, GLFW shell or examples.
    --disable-desktop-embeddings --no-build-glfw-shell --no-build-embedder-examples
    --no-enable-unittests
)
./flutter/tools/gn "${gn_args[@]}"
# clang_x64/gen_snapshot is gen_snapshot built for the x64 host that emits
# arm64 code; the default-toolchain gen_snapshot would be an arm64 binary.
# The :flutter_engine group is empty when cross-compiling (it only builds
# the library for the host toolchain), so name the library itself, as
# Google's embedder-archive target does.
ninja -C "out/$target" \
    flutter/shell/platform/embedder:flutter_engine_library \
    flutter/shell/platform/embedder:copy_headers \
    clang_x64/gen_snapshot

o=out/$target
# tools/gn turns on the embedder's Vulkan renderer (kVulkan) for Linux; the
# GL path stays too. Fail loudly if a future release stops doing that.
grep -q 'shell_enable_vulkan = true' "$o/args.gn" || { echo "engine built without Vulkan" >&2; exit 1; }
strip=flutter/buildtools/linux-x64/clang/bin/llvm-strip
kit=$work/kit-$mode
rm -rf "$kit" "$kit-symbols"
mkdir -p "$kit/usr/lib" "$kit/usr/share/flutter" "$kit/include" "$kit/host/linux-x64" "$kit-symbols"
cp "$o/libflutter_engine.so" "$kit-symbols/libflutter_engine.so"
"$strip" --strip-unneeded -o "$kit/usr/lib/libflutter_engine.so" "$o/libflutter_engine.so"
cp "$o/flutter_embedder.h" "$kit/include/"
cp flutter/third_party/icu/flutter/icudtl.dat "$kit/usr/share/flutter/"
"$strip" --strip-unneeded -o "$kit/host/linux-x64/gen_snapshot" "$o/clang_x64/gen_snapshot"
printf '%s\n' "$version" > "$kit/flutter-version"
printf '%s\n' "$rev" > "$kit/engine-revision"
printf '%s\n' "$mode" > "$kit/runtime-mode"
cp "$o/args.gn" "$kit/gn-args"
cp "$kit"/{flutter-version,engine-revision,runtime-mode} "$kit-symbols/"

name=aera-flutter-engine-arm64-$mode-$version
tar -C "$kit" -cJf "$out/$name.tar.xz" .
tar -C "$kit-symbols" -cJf "$out/$name-symbols.tar.xz" .
ls -l "$out"
