#!/usr/bin/env bash
# Build Mesa 26.2.2 for the AERA runtime: EGL + GLES through Zink on Turnip
# (KGSL), plus softpipe for emulated tests. Installs into <destdir>/usr.
# Run on arm64, or pass a meson cross file as the second argument.
#
#   tools/build_mesa.sh <destdir> [cross-file]
set -euo pipefail
dest=$(realpath -m "$1") cross=${2:-}
here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
sha=eeb29ca7e56cfaa8e8a79538dcf834e3b18e501c31bef5145e959ea437cc4216
curl -fsSL -o "$work/mesa.tar.xz" http://archive.ubuntu.com/ubuntu/pool/main/m/mesa/mesa_26.2.2.orig.tar.xz ||
    curl -fsSL -o "$work/mesa.tar.xz" https://archive.mesa3d.org/mesa-26.2.2.tar.xz
echo "$sha  $work/mesa.tar.xz" | sha256sum -c -
tar -C "$work" -xf "$work/mesa.tar.xz"
cd "$work/mesa-26.2.2"
patch -p1 < "$here/third_party/mesa/mesa-26.2.2-zink-kgsl-surfaceless.patch"
meson setup build --wrap-mode=nodownload ${cross:+--cross-file "$(realpath "$cross")"} --prefix=/usr --libdir=lib \
    -Dbuildtype=release -Db_ndebug=true -Dplatforms= -Degl=enabled -Dgles1=disabled \
    -Dgles2=enabled -Dopengl=true -Dglx=disabled -Dgbm=disabled -Dglvnd=disabled \
    -Dgallium-drivers=zink,softpipe -Dvulkan-drivers=freedreno -Dfreedreno-kmds=msm,kgsl \
    -Dllvm=disabled -Dvalgrind=disabled -Dlibunwind=disabled -Dlmsensors=disabled \
    -Dbuild-tests=false -Dvideo-codecs= -Dvulkan-layers= -Dtools= -Dzstd=enabled \
    -Dexpat=enabled -Dteflon=false
ninja -C build
DESTDIR="$dest" ninja -C build install
