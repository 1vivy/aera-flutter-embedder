# aera-flutter-embedder (generic-host branch)

Runs Flutter apps **inside AERA Recovery**, drawn by the phone's GPU, on the
generic pixel + GPU plugin host an AERA maintainer is adding.

> **The host is not released yet.** This branch is written against an
> assumed interface so it is ready to switch the day the official one lands.
> Every guess lives in [`src/host.rs`](src/host.rs) and in the manifest
> constants at the top of [`tools/make_aerap.py`](tools/make_aerap.py), each
> marked `ASSUMED`. What we need from the host is tracked in
> [aera-flutter-demo#1](https://github.com/1vivy/aera-flutter-demo/issues/1).
> The `main` branch keeps the working stopgap that borrows AERA Browser's slot.

## How it runs

The app is an ordinary Host API 2 style plugin with its own ID
(`type: "ui-runtime"`, `entry: "main"`, `executable: "usr/bin/aera-plugin"`),
so it installs next to AERA Browser instead of replacing it, and AERA draws no
browser chrome over it.

1. AERA extracts the payload and starts `usr/bin/aera-plugin
   --aera-host-api=3` with its control channel on fd 4 and, assumed, a pixel
   surface memfd on fd 3.
2. `aera-plugin` is a small static program. It finds the runtime around
   itself, binds the payload's fonts at `/usr/share/fonts` in a private mount
   namespace, points Mesa and the Vulkan loader into the payload, and execs
   `usr/bin/aera-flutter` through the runtime's own glibc loader.
3. `aera-flutter` handshakes (`HELLO` → `HELLO_ACK` with the pixel surface
   feature → `SURFACE` with width, height, stride, slots and scale), renders
   with Flutter's OpenGL backend on surfaceless EGL (Mesa Zink → Turnip →
   `/dev/kgsl-3d0`), copies each frame into a free slot and sends `PRESENT`.
   AERA answers `FRAME_DONE`. Touches, keys, Back and lifecycle arrive on the
   channel.

Like every generic plugin, the app is a recovery module: it runs as root in
recovery's own namespaces with recovery's full access. There is no jail; the
browser jail stays AERA Browser's alone.

## Pieces

| Path | What |
| --- | --- |
| `src/host.rs` | the assumed host interface: messages, handshake, surface |
| `src/bin/aera_flutter.rs` | `aera-flutter`, the embedder |
| `launcher/` | `aera-plugin`, the static launcher AERA starts |
| `src/bin/host_sim.rs` | `aera-host-sim`, the assumed host on a PC; writes frames as PNGs and replays taps, keys, Back and lifecycle |
| `tools/assemble_runtime.py` | builds the arm64 runtime: glibc, Flutter engine, Mesa, fonts, launcher and embedder, as real files resolved by soname |
| `tools/make_aerap.py` | packs a staged payload into an installable `.aerap` under the app's own ID |
| `tools/package_kits.sh` | makes the runtime and simulator kits, published as `generic-host-flutter-<version>` releases |

## Build

```sh
cargo build --release                                   # PC: embedder + simulator
RUSTFLAGS="-C target-feature=+crt-static" \
    cargo build --release -p aera-plugin --target x86_64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu \
    --no-default-features --bin aera-flutter            # phone
RUSTFLAGS="-C target-feature=+crt-static" \
    cargo build --release -p aera-plugin --target aarch64-unknown-linux-gnu
```

Mesa 26.2.2 is built for arm64 with `-Dgallium-drivers=zink,softpipe
-Dvulkan-drivers=freedreno -Dfreedreno-kmds=msm,kgsl -Dplatforms=` and AERA
Browser's `mesa-26.2.2-zink-kgsl-surfaceless.patch`.

## Switching to the official host

1. Replace the `ASSUMED` constants and kinds in `src/host.rs` with the real
   ones, and adjust `connect()` if the handshake differs.
2. Replace the `ASSUMED` manifest constants in `tools/make_aerap.py`.
3. Run `cargo test` and the simulator, then the Kits workflow on this branch.

## Limits

- The engine is Google's debug (JIT) embedder build, so apps are debug builds
  and must use exactly the Flutter release named in the kit.
- The Flutter engine only looks for fonts in `/usr/share/fonts`; `aera-plugin`
  binds the payload's fonts there in its own mount namespace.
