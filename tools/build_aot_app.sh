#!/usr/bin/env bash
# Compile a Flutter app into an arm64 libapp.so for a release or profile
# engine from tools/build_engine.sh. Run it in the app's project directory
# with the Flutter release the engine was built from on PATH.
#
#   tools/build_aot_app.sh <release|profile> <engine-kit-dir> <out-dir>
#
# Writes <out-dir>/libapp.so and <out-dir>/flutter_assets. The same steps as
# `flutter build linux --release`, minus the GTK runner: frontend_server
# turns the Dart code into a tree-shaken kernel, gen_snapshot turns that into
# ELF machine code.
set -euo pipefail

mode=$1 kit=$(realpath "$2") out=$(realpath -m "$3")
case $mode in
release) sdk=flutter_patched_sdk_product defines=(-Ddart.vm.profile=false -Ddart.vm.product=true) ;;
profile) sdk=flutter_patched_sdk defines=(-Ddart.vm.profile=true -Ddart.vm.product=false) ;;
*) echo "mode must be release or profile" >&2; exit 2 ;;
esac

want=$(cat "$kit/flutter-version")
have=$(flutter --version --machine | python3 -c 'import json,sys; print(json.load(sys.stdin)["frameworkVersion"])')
if [ "$want" != "$have" ]; then
    echo "The engine was built for Flutter $want but flutter is $have" >&2
    exit 1
fi
if [ "$(cat "$kit/runtime-mode")" != "$mode" ]; then
    echo "The engine kit is a $(cat "$kit/runtime-mode") engine, not $mode" >&2
    exit 1
fi

flutter_root=$(dirname "$(dirname "$(readlink -f "$(command -v flutter)")")")
dart_sdk=$flutter_root/bin/cache/dart-sdk
package=$(python3 -c 'import re; print(re.search(r"^name:\s*(\S+)", open("pubspec.yaml").read(), re.M).group(1))')

mkdir -p "$out" build/aera-aot
flutter build bundle --"$mode" --asset-dir "$out/flutter_assets" >&2

"$dart_sdk/bin/dartaotruntime" "$dart_sdk/bin/snapshots/frontend_server_aot.dart.snapshot" \
    --sdk-root "$flutter_root/bin/cache/artifacts/engine/common/$sdk/" \
    --target=flutter --no-print-incremental-dependencies \
    "${defines[@]}" \
    --delete-tostring-package-uri=dart:ui --delete-tostring-package-uri=package:flutter \
    --aot --tfa --target-os linux \
    --packages .dart_tool/package_config.json \
    --output-dill build/aera-aot/app.dill \
    --verbosity=error \
    "package:$package/main.dart"

"$kit/host/linux-x64/gen_snapshot" --deterministic --strip \
    --snapshot_kind=app-aot-elf --elf="$out/libapp.so" \
    build/aera-aot/app.dill

ls -l "$out/libapp.so"
