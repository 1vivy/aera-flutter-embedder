#!/usr/bin/env python3
"""Pack a staged AERA Flutter payload into an installable .aerap.

The package uses the `browser` ID with the `browser-runtime` type, which is
the only AERA host that gives a plugin the GPU. AERA installs it as an
unofficial app after a warning; it replaces AERA Browser on that device until
the official browser is reinstalled.

    tools/make_aerap.py --stage build/stage --name "My App" --version 0.1.0 \\
        --description "What it does" --out build/My-App-0.1.0.aerap

The payload format (AERAWEB1 + xz with the ARM64 filter) and limits follow
aeraui/features/browser/runtime.cpp in AERA-Recovery/android_bootable_recovery.
"""
import argparse
import hashlib
import json
import shutil
import struct
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

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
    parser.add_argument("--name", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--description", default="A Flutter app for AERA Recovery.")
    parser.add_argument("--payload-url", default="https://example.invalid/runtime.xz",
                        help="where runtime.xz is published; unused for local installs")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    if not (args.stage / "usr/bin/aera-browser-worker").is_file():
        sys.exit("stage has no usr/bin/aera-browser-worker")
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
            "id": "browser",
            "name": args.name,
            "version": args.version,
            "description": args.description,
            "type": "browser-runtime",
            "entry": "browser",
            "min_host_api": 1,
            "payload": "runtime.xz",
            "payload_url": args.payload_url,
            **sizes,
            "permissions": ["network", "display", "temporary-memory", "audio-output",
                            "gpu-acceleration", "download-storage"],
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
