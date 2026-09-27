//! `aera-plugin`: the executable AERA's plugin host starts.
//!
//! Host API 2 execs `usr/bin/aera-plugin` directly and, if that fails, only
//! knows how to fall back to a packaged musl loader. The Flutter runtime is
//! glibc, and generic plugins run in recovery's own filesystem, where the
//! payload's `/lib/ld-linux-aarch64.so.1` is not where the kernel looks. So this tiny
//! static program is what AERA starts: it finds the runtime around itself,
//! points Mesa and the Vulkan loader into it, and execs `usr/bin/aera-flutter`
//! through the runtime's own loader. Descriptors (the control channel and
//! the pixel surface) and arguments pass through untouched.
//!
//! Flutter's engine only reads fonts from `/usr/share/fonts/`. Plugins run
//! as root, so the launcher gives itself a private mount namespace and binds
//! the payload's fonts there; recovery's own mounts are not changed.
//!
//! Build it static so the kernel needs no loader to start it:
//! `RUSTFLAGS="-C target-feature=+crt-static" cargo build --release -p aera-plugin --target <triple>`

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

fn runtime_root() -> PathBuf {
    if let Some(root) = std::env::var_os("AERA_PLUGIN_ROOT").filter(|r| !r.is_empty()) {
        return root.into();
    }
    // <root>/usr/bin/aera-plugin
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent()?.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| "/".into())
}

fn set_default(name: &str, value: &Path) {
    if std::env::var_os(name).is_none() {
        std::env::set_var(name, value);
    }
}

fn cstring(value: &OsStr) -> CString {
    CString::new(value.as_bytes()).expect("argument with NUL")
}

/// Makes `/usr/share/fonts` show the payload's fonts, in a mount namespace
/// only this plugin (and its children) see. Best effort: text is missing,
/// not the app, when it fails.
fn bind_fonts(root: &Path) {
    let fonts = root.join("usr/share/fonts");
    let target = Path::new("/usr/share/fonts");
    if !fonts.is_dir() || root == Path::new("/") || unsafe { libc::geteuid() } != 0 {
        return;
    }
    let path = |p: &Path| cstring(p.as_os_str());
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0
            || libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) != 0
        {
            eprintln!("aera-plugin: no private mount namespace for fonts: {}", std::io::Error::last_os_error());
            return;
        }
    }
    // Recovery's rootfs is a ramdisk, so a created mount point is gone at
    // the next boot.
    if std::fs::create_dir_all(target).is_err() {
        return;
    }
    let (source, target) = (path(&fonts), path(target));
    if unsafe { libc::mount(source.as_ptr(), target.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) } != 0 {
        eprintln!("aera-plugin: could not bind fonts: {}", std::io::Error::last_os_error());
    }
}

fn main() {
    let root = runtime_root();
    bind_fonts(&root);
    set_default("AERA_PLUGIN_ROOT", &root);
    let icd = root.join("usr/share/vulkan/icd.d/freedreno_icd.json");
    if icd.is_file() {
        set_default("VK_DRIVER_FILES", &icd);
    }
    let drirc = root.join("usr/share/drirc.d");
    if drirc.is_dir() {
        set_default("DRIRC_CONFIGDIR", &drirc);
    }

    let program = root.join("usr/bin/aera-flutter");
    let loader = ["ld-linux-aarch64.so.1", "ld-linux-x86-64.so.2"]
        .iter()
        .map(|name| root.join("lib").join(name))
        .find(|path| path.is_file());
    let mut argv: Vec<OsString> = Vec::new();
    if let Some(loader) = &loader {
        let libraries = format!("{}:{}", root.join("usr/lib").display(), root.join("lib").display());
        argv.extend([loader.clone().into(), "--library-path".into(), libraries.into()]);
    }
    argv.push(program.clone().into());
    argv.extend(std::env::args_os().skip(1));

    let path = cstring(loader.as_deref().unwrap_or(&program).as_os_str());
    let argv: Vec<CString> = argv.iter().map(|a| cstring(a)).collect();
    let mut pointers: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    pointers.push(std::ptr::null());
    unsafe { libc::execv(path.as_ptr(), pointers.as_ptr()) };
    eprintln!("aera-plugin: could not start {}: {}", path.to_string_lossy(), std::io::Error::last_os_error());
    std::process::exit(78);
}
