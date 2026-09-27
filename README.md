# aera-flutter-embedder

Runs Flutter apps **inside AERA Recovery**, drawn by the phone's GPU.

AERA gives the GPU only to its browser slot, so this embedder speaks AERA
Browser's worker protocol: AERA starts it as `/usr/bin/aera-browser-worker
--isolated-ipc-v1` inside the browser jail, with a shared frame buffer on fd 3
(two 1080x2100 BGRA slots) and a `SOCK_SEQPACKET` control socket on fd 4
(`aeraui/features/browser/protocol.hpp` in AERA-Recovery/android_bootable_recovery).
The embedder renders with Flutter's OpenGL backend on surfaceless EGL, which
Mesa's Zink turns into Vulkan on Turnip over `/dev/kgsl-3d0`, then copies each
frame into AERA's buffer. Touches, keys, Back and Close arrive on the socket.

App developers don't need this repo directly: start from
[aera-flutter-template](https://github.com/1vivy/aera-flutter-template), which
downloads the kits built here.

## Pieces

| Path | What |
| --- | --- |
| `src/main.rs` | `aera-browser-worker`, the embedder |
| `src/bin/host_sim.rs` | `aera-host-sim`, AERA's side of the protocol on a PC; writes frames as PNGs and replays taps and keys |
| `tools/assemble_runtime.py` | builds the arm64 runtime: glibc, Flutter engine, Mesa, fonts and the embedder, as real files resolved by soname |
| `tools/make_aerap.py` | packs a staged payload into an installable `.aerap` (`browser` ID, `browser-runtime` type) |
| `tools/package_kits.sh` | makes the runtime and simulator kits published as releases (`flutter-<version>` tags) |

## Build

```sh
cargo build --release                                   # PC: worker + simulator
cargo build --release --target aarch64-unknown-linux-gnu \
    --no-default-features --bin aera-browser-worker     # phone
```

Mesa 26.2.2 is built for arm64 with `-Dgallium-drivers=zink,softpipe
-Dvulkan-drivers=freedreno -Dfreedreno-kmds=msm,kgsl -Dplatforms=` and AERA
Browser's `mesa-26.2.2-zink-kgsl-surfaceless.patch`.

## Limits

- The engine is Google's debug (JIT) embedder build, so apps are debug builds
  and must use exactly the Flutter release named in the kit. Release (AOT)
  needs a custom engine build.
- Installing replaces AERA Browser on that phone until the browser is
  reinstalled, and AERA's browser top bar stays above the app.
- The jail denies `listen()`, so the Dart VM service is off.
