//! `aera-browser-worker`: runs a Flutter app in AERA Recovery's browser slot.
//!
//! AERA's `aera-browser-jail --browser` starts `/usr/bin/aera-browser-worker
//! --isolated-ipc-v1` inside a chroot of the extracted payload, with the frame
//! memory on fd 3 and the control socket on fd 4. The same binary runs under
//! `aera-host-sim` on a PC with identical descriptors.

use aera_flutter_embedder::{bridge, engine};

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 || args[1] != "--isolated-ipc-v1" {
        eprintln!("aera-browser-worker must be started by AERA with --isolated-ipc-v1");
        std::process::exit(78);
    }
    let frames = match bridge::Frames::map(bridge::FRAME_FD) {
        Ok(frames) => frames,
        Err(error) => {
            eprintln!("aera-flutter: invalid frame memory: {error}");
            std::process::exit(78);
        }
    };
    unsafe { libc::close(bridge::FRAME_FD) };
    // SAFETY: AERA hands fd 4 to this process and nothing else owns it.
    let control = unsafe { bridge::Control::from_fd(bridge::CONTROL_FD) };

    // The payload root is `/` inside AERA's jail. The simulator points this
    // at an unpacked payload directory instead.
    let root = PathBuf::from(std::env::var_os("AERA_FLUTTER_ROOT").unwrap_or_else(|| "/".into()));
    let mut config = engine::default_config(&root);
    if let Some(args) = std::env::var_os("AERA_FLUTTER_ENGINE_ARGS") {
        config.extra_engine_args = args.to_string_lossy().split_whitespace().map(str::to_owned).collect();
    }

    // A second handle on the control socket, so a startup failure can still
    // tell AERA why after the embedder has consumed the first.
    let errors = unsafe { bridge::Control::from_fd(libc::fcntl(bridge::CONTROL_FD, libc::F_DUPFD_CLOEXEC, 10)) };
    let embedder = match engine::Embedder::start(config, frames, control) {
        Ok(embedder) => embedder,
        Err(error) => {
            eprintln!("aera-flutter: {error}");
            let mut packet = bridge::Packet::new(bridge::kind::ERROR);
            packet.text = format!("Flutter could not start: {error}");
            let _ = errors.send(&packet);
            std::process::exit(78);
        }
    };
    drop(errors);
    std::process::exit(embedder.run());
}
