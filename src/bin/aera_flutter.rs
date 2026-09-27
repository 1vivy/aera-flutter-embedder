//! `aera-flutter`: runs a Flutter app on AERA's pixel surface.
//!
//! `aera-plugin` starts it with AERA's control channel and surface still on
//! their descriptors. The same binary runs under `aera-host-sim` on a PC.

use aera_flutter_embedder::{engine, host};

use std::os::fd::RawFd;
use std::path::PathBuf;
use std::time::Duration;

fn fd_from_env(name: &str, default: RawFd) -> RawFd {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let api = format!("--aera-host-api={}", host::HOST_API);
    if !std::env::args().skip(1).any(|arg| arg == api) {
        eprintln!("aera-flutter must be started by AERA with {api}");
        std::process::exit(78);
    }
    let control_fd = fd_from_env(host::CONTROL_FD_ENV, host::CONTROL_FD);
    let surface_fd = fd_from_env(host::SURFACE_FD_ENV, host::SURFACE_FD);
    // SAFETY: AERA hands the control descriptor to this process alone.
    let control = unsafe { host::Control::from_fd(control_fd) };
    // A second handle, so a startup failure can still tell AERA why after the
    // handshake or the engine has consumed the first.
    let errors = control.try_clone().ok();
    let fail = |error: String| -> ! {
        eprintln!("aera-flutter: {error}");
        if let Some(errors) = &errors {
            let _ = errors.send(&host::Message::status(&format!("Flutter could not start: {error}")));
        }
        std::process::exit(78);
    };

    let session = host::connect(control, surface_fd, Duration::from_secs(10)).unwrap_or_else(|e| fail(e));
    unsafe { libc::close(surface_fd) };

    let root = PathBuf::from(std::env::var_os(host::ROOT_ENV).unwrap_or_else(|| "/".into()));
    let mut config = engine::default_config(&root);
    if let Some(args) = std::env::var_os("AERA_FLUTTER_ENGINE_ARGS") {
        config.extra_engine_args.extend(args.to_string_lossy().split_whitespace().map(str::to_owned));
    }
    let embedder = engine::Embedder::start(config, session).unwrap_or_else(|e| fail(e));
    drop(errors);
    std::process::exit(embedder.run());
}
