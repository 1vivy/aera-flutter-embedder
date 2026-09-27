#!/usr/bin/env python3
"""Assemble the AERA Flutter runtime: everything a Flutter app needs inside
AERA's generic pixel + GPU plugin host except the app itself.

The payload is a complete userspace that runs from wherever AERA extracts it
(generic plugins run in recovery's own filesystem, not a chroot): the glibc loader and libraries, the Flutter engine, Mesa (EGL, GLES,
Zink, Turnip KGSL), the Vulkan loader, fonts, the static launcher at
/usr/bin/aera-plugin (what AERA starts) and the embedder at
/usr/bin/aera-flutter. Shared libraries are copied as real files under
their sonames (the AERA payload format carries no symlinks), and every
DT_NEEDED entry is resolved from the given sysroots so nothing is missing at
run time.

    tools/assemble_runtime.py --out build/runtime \\
        --launcher target/aarch64-unknown-linux-gnu/release/aera-plugin \\
        --embedder target/aarch64-unknown-linux-gnu/release/aera-flutter \\
        --engine engine/linux-arm64/libflutter_engine.so \\
        --icu engine/icudtl.dat --mesa mesa/stage \\
        --sysroot /usr/lib/aarch64-linux-gnu --sysroot /lib/aarch64-linux-gnu \\
        --fonts /usr/share/fonts/truetype/roboto/unhinted/RobotoTTF
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

# glibc pieces live in /lib, where the ELF interpreter path points.
GLIBC = {"ld-linux-aarch64.so.1", "libc.so.6", "libm.so.6", "libdl.so.2",
         "libpthread.so.0", "librt.so.1", "libresolv.so.2"}
# Loaded with dlopen, so no DT_NEEDED entry names them.
DLOPENED = ["libvulkan.so.1", "libGLESv2.so.2"]


def needed(path, readelf):
    output = subprocess.run([readelf, "-d", str(path)], check=True,
                            capture_output=True, text=True).stdout
    return [line.split("[", 1)[1].rstrip("]")
            for line in output.splitlines() if "(NEEDED)" in line]


def find(name, search):
    for directory in search:
        candidate = Path(directory) / name
        if candidate.exists():
            return candidate.resolve()
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--launcher", type=Path, required=True,
                        help="static aera-plugin (built with +crt-static)")
    parser.add_argument("--embedder", type=Path, required=True)
    parser.add_argument("--engine", type=Path, required=True)
    parser.add_argument("--icu", type=Path, required=True)
    parser.add_argument("--mesa", type=Path, required=True,
                        help="Mesa install tree (DESTDIR) with usr/lib and usr/share")
    parser.add_argument("--sysroot", action="append", default=[],
                        help="directory to resolve system libraries from (repeatable)")
    parser.add_argument("--fonts", action="append", default=[],
                        help="directory of .ttf/.otf files to ship in /usr/share/fonts")
    parser.add_argument("--readelf", default="aarch64-linux-gnu-readelf")
    args = parser.parse_args()

    out = args.out
    if out.exists() and any(out.iterdir()):
        sys.exit(f"{out} is not empty")
    lib, usr_lib = out / "lib", out / "usr/lib"
    for directory in (lib, usr_lib, out / "usr/bin", out / "usr/share/flutter",
                      out / "usr/share/fonts", out / "usr/share/vulkan/icd.d"):
        directory.mkdir(parents=True, exist_ok=True)

    for source, name in ((args.launcher, "aera-plugin"), (args.embedder, "aera-flutter")):
        shutil.copy2(source, out / "usr/bin" / name)
        os.chmod(out / "usr/bin" / name, 0o755)
    shutil.copy2(args.engine, usr_lib / "libflutter_engine.so")
    shutil.copy2(args.icu, out / "usr/share/flutter/icudtl.dat")

    mesa_lib = args.mesa / "usr/lib"
    search = [mesa_lib] + args.sysroot
    for name in ("libEGL.so.1", "libGLESv2.so.2", "libvulkan_freedreno.so"):
        shutil.copy2(find(name, [mesa_lib]), usr_lib / name)
    for entry in (mesa_lib).iterdir():
        if entry.name.startswith("libgallium-") and entry.suffix == ".so":
            shutil.copy2(entry, usr_lib / entry.name)
    # aera-plugin points VK_DRIVER_FILES here. The driver path is made
    # relative to the JSON file so it resolves wherever AERA extracts the
    # payload.
    icds = list((args.mesa / "usr/share/vulkan/icd.d").glob("freedreno_icd*.json"))
    icd = json.loads(icds[0].read_text())
    icd["ICD"]["library_path"] = "../../../lib/libvulkan_freedreno.so"
    (out / "usr/share/vulkan/icd.d/freedreno_icd.json").write_text(json.dumps(icd, indent=4) + "\n")
    if (args.mesa / "usr/share/drirc.d").is_dir():
        shutil.copytree(args.mesa / "usr/share/drirc.d", out / "usr/share/drirc.d")

    for name in DLOPENED:
        if not (usr_lib / name).exists():
            source = find(name, search)
            if source is None:
                sys.exit(f"missing {name}")
            shutil.copy2(source, usr_lib / name)

    # Resolve the DT_NEEDED closure of every ELF in the tree. The static
    # launcher needs nothing.
    pending = [p for p in out.rglob("*") if p.is_file() and p.read_bytes()[:4] == b"\x7fELF"
               and p.name != "aera-plugin"]
    seen = set()
    while pending:
        path = pending.pop()
        for name in needed(path, args.readelf):
            if name in seen:
                continue
            seen.add(name)
            target = (lib if name in GLIBC else usr_lib) / name
            if target.exists():
                continue
            source = find(name, search)
            if source is None:
                sys.exit(f"{path.name} needs {name}, which no sysroot provides")
            shutil.copy2(source, target)
            pending.append(target)

    for directory in args.fonts:
        for font in sorted(Path(directory).rglob("*")):
            if font.suffix.lower() in (".ttf", ".otf"):
                shutil.copy2(font, out / "usr/share/fonts" / font.name)

    total = sum(p.stat().st_size for p in out.rglob("*") if p.is_file())
    count = sum(1 for p in out.rglob("*") if p.is_file())
    print(f"runtime: {count} files, {total / 1e6:.1f} MB in {out}")


if __name__ == "__main__":
    main()
