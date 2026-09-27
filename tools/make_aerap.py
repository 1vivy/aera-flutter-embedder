#!/usr/bin/env python3
"""Pack a staged AERA Flutter payload into an installable .aerap.

The package targets AERA's generic pixel + GPU plugin host, which an AERA
maintainer is adding and which is not released yet. The manifest fields that
depend on it are the ASSUMED constants below; everything else (ID rules,
limits, the payload format) is what AERA's plugin manager already enforces
for Host API 2 plugins. The app keeps its own ID, so it installs next to AERA
Browser instead of replacing it.

    tools/make_aerap.py --stage build/stage --id org.example.app --name "My App" \\
        --version 0.1.0 --description "What it does" --out build/My-App-0.1.0.aerap

Add --privileged to ask for the opt-in privileged mode (see PRIVILEGED).

The payload format (AERAWEB1 + xz with the ARM64 filter) and limits follow
aeraui/features/browser/runtime.cpp, and the manifest checks follow
aeraui/features/plugins/plugin_manager.cpp, in
AERA-Recovery/android_bootable_recovery (branch aera-16.0).
"""
import argparse
import hashlib
import json
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

# Host API 2's generic manifest (plugin_manager.cpp), which the pixel host is
# assumed to extend.
TYPE = "ui-runtime"
ENTRY = "main"
EXECUTABLE = "usr/bin/aera-plugin"
# ASSUMED: the pixel host is Host API 3 with protocol version 3.
HOST_API = 3
PROTOCOL_VERSION = 3
# `display` and `touch-input` are required by Host API 2. The rest are ASSUMED
# names for what the pixel host will grant.
PERMISSIONS = ["display", "touch-input", "pixel-surface", "gpu-acceleration",
               "audio-output", "network"]
# ASSUMED: opting in to the privileged mode (root, recovery's filesystem and
# devices, like Host API 2 plugins today) is one more permission.
PRIVILEGED = "privileged"

MAX_MEMBERS = 4096
MAX_MEMBER_BYTES = 100 * 1024 * 1024
MAX_PAYLOAD = 512 * 1024 * 1024


def sha256(source):
    digest = hashlib.sha256()
    while block := source.read(1024 * 1024):
        digest.update(block)
    return digest.hexdigest()


def safe(name):
    return (0 < len(name.encode()) < 240 and not name.startswith("/")
            and all(part not in ("", ".", "..") for part in name.split("/")))


def pack(stage, payload):
    files = sorted(p for p in stage.rglob("*") if p.is_file() and not p.is_symlink())
    links = [p for p in stage.rglob("*") if p.is_symlink()]
    if links:
        sys.exit(f"symlinks are not supported in the payload: {links[0]}")
    if not files or len(files) > MAX_MEMBERS:
        sys.exit(f"payload must hold 1 to {MAX_MEMBERS} files, not {len(files)}")
    with tempfile.TemporaryFile() as expanded:
        expanded.write(b"AERAWEB1" + struct.pack("<I", len(files)))
        for path in files:
            name = path.relative_to(stage).as_posix()
            size = path.stat().st_size
            if not safe(name) or size > MAX_MEMBER_BYTES:
                sys.exit(f"payload member not allowed by AERA: {name} ({size} bytes)")
            mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
            encoded = name.encode()
            expanded.write(struct.pack("<HHQ", len(encoded), mode, size))
            expanded.write(encoded)
            expanded.write(b"\0" * (-expanded.tell() % 4))
            with path.open("rb") as source:
                shutil.copyfileobj(source, expanded, 1024 * 1024)
        expanded_size = expanded.tell()
        expanded.seek(0)
        expanded_sha256 = sha256(expanded)
        expanded.seek(0)
        with payload.open("wb") as target:
            subprocess.run(["xz", "-c", "--threads=1", "--check=crc32", "--arm64",
                            "--lzma2=preset=9e,lc=2,lp=2"],
                           stdin=expanded, stdout=target, check=True)
    with payload.open("rb") as source:
        payload_sha256 = sha256(source)
    return {"payload_size": payload.stat().st_size, "payload_sha256": payload_sha256,
            "expanded_size": expanded_size, "expanded_sha256": expanded_sha256,
            "member_count": len(files)}


def verify(payload, sizes):
    """Re-reads the payload the way AERA's extractor does."""
    import lzma
    with lzma.open(payload) as stream:
        data = stream.read()
    if len(data) != sizes["expanded_size"] or hashlib.sha256(data).hexdigest() != sizes["expanded_sha256"]:
        sys.exit("payload self-check failed: expanded size or hash differs")
    if data[:8] != b"AERAWEB1" or struct.unpack_from("<I", data, 8)[0] != sizes["member_count"]:
        sys.exit("payload self-check failed: bad header")
    offset = 12
    for _ in range(sizes["member_count"]):
        length, mode, size = struct.unpack_from("<HHQ", data, offset)
        offset += 12
        name = data[offset:offset + length].decode()
        offset += length
        offset += -offset % 4
        if not safe(name) or mode not in (0o644, 0o755) or size > MAX_MEMBER_BYTES:
            sys.exit(f"payload self-check failed at {name}")
        offset += size
    if offset != len(data):
        sys.exit("payload self-check failed: trailing bytes")


def add_file(archive, name, data):
    info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_STORED
    info.create_system = 3
    info.external_attr = 0o100444 << 16
    with archive.open(info, "w", force_zip64=True) as output:
        if isinstance(data, Path):
            with data.open("rb") as source:
                shutil.copyfileobj(source, output, 1024 * 1024)
        else:
            output.write(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--stage", type=Path, required=True)
    parser.add_argument("--id", required=True,
                        help="plugin ID: lowercase letters, digits, '-' and '.', up to 64")
    parser.add_argument("--name", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--description", default="A Flutter app for AERA Recovery.")
    parser.add_argument("--payload-url", default="https://example.invalid/runtime.xz",
                        help="where runtime.xz is published; unused for local installs")
    parser.add_argument("--privileged", action="store_true",
                        help="ask for the opt-in privileged mode")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    for program in (EXECUTABLE, "usr/bin/aera-flutter"):
        if not (args.stage / program).is_file():
            sys.exit(f"stage has no {program}")
    if (not re.fullmatch(r"[a-z0-9.-]{1,64}", args.id) or args.id[0] == "." or args.id[-1] == "."
            or args.id == "browser"):
        sys.exit(f"plugin ID {args.id!r} is not allowed")
    if not (args.stage / "usr/share/flutter/flutter_assets").is_dir():
        sys.exit("stage has no usr/share/flutter/flutter_assets")
    if len(args.name) > 80 or len(args.description) > 320 or len(args.version) > 32:
        sys.exit("name, description or version exceeds AERA's limits")

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as work:
        payload = Path(work) / "runtime.xz"
        sizes = pack(args.stage, payload)
        verify(payload, sizes)
        if sizes["payload_size"] > MAX_PAYLOAD:
            sys.exit("payload exceeds AERA's 512 MiB limit")
        manifest = {
            "schema": 1,
            "id": args.id,
            "name": args.name,
            "version": args.version,
            "description": args.description,
            "type": TYPE,
            "entry": ENTRY,
            "min_host_api": HOST_API,
            "protocol_version": PROTOCOL_VERSION,
            "executable": EXECUTABLE,
            "payload": "runtime.xz",
            "payload_url": args.payload_url,
            **sizes,
            "permissions": PERMISSIONS + ([PRIVILEGED] if args.privileged else []),
        }
        manifest_bytes = (json.dumps(manifest, indent=2) + "\n").encode()
        temporary = args.out.with_suffix(args.out.suffix + ".new")
        with zipfile.ZipFile(temporary, "w", allowZip64=True) as archive:
            add_file(archive, "plugin.json", manifest_bytes)
            add_file(archive, "runtime.xz", payload)
        temporary.replace(args.out)
        (args.out.parent / "plugin.json").write_bytes(manifest_bytes)
    print(json.dumps({"package": str(args.out), "size": args.out.stat().st_size, **sizes}, indent=2))


if __name__ == "__main__":
    main()
