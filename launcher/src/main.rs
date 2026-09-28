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
//! Flutter's engine only reads fonts from `/usr/share/fonts/` and Dart only
//! trusts `/etc/ssl/certs/ca-certificates.crt`. Plugins run as root, so the
//! launcher gives itself a private mount namespace and binds the payload's
//! fonts and CA bundle there; recovery's own mounts are not changed.
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

/// Gives this plugin (and its children) a private mount namespace, so the
/// binds below never change recovery's own mounts. Plugins run as root.
fn private_namespace(root: &Path) -> bool {
    if root == Path::new("/") || unsafe { libc::geteuid() } != 0 {
        return false;
    }
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0
            || libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) != 0
        {
            eprintln!("aera-plugin: no private mount namespace: {}", std::io::Error::last_os_error());
            return false;
        }
    }
    true
}

/// Binds the payload's `source` over `target`, creating the mount point
/// (a directory or an empty file, like `source`) if it is missing. Recovery's
/// rootfs is a ramdisk, so a created mount point is gone at the next boot.
/// Symlinks on the way are followed: recovery's `/etc` points at
/// `/system/etc`. Best effort.
fn bind(source: &Path, target: &Path) {
    let created = if source.is_dir() {
        std::fs::create_dir_all(target)
    } else {
        target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::OpenOptions::new().create(true).append(true).open(target).map(drop))
    };
    let resolved = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    if let Err(error) = created {
        eprintln!("aera-plugin: no mount point at {}: {error}", resolved.display());
        return;
    }
    let (from, to) = (cstring(source.as_os_str()), cstring(resolved.as_os_str()));
    if unsafe { libc::mount(from.as_ptr(), to.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) } != 0 {
        eprintln!("aera-plugin: could not bind {}: {}", resolved.display(), std::io::Error::last_os_error());
    }
}

/// Flutter's engine only reads fonts from `/usr/share/fonts/`, and Dart's
/// HttpClient only trusts `/etc/ssl/certs/ca-certificates.crt` (it ignores
/// SSL_CERT_FILE). Recovery has neither, so the payload's copies are bound
/// there. Without them text or HTTPS is missing, not the app.
fn bind_runtime_files(root: &Path) {
    let files = [("usr/share/fonts", "/usr/share/fonts"), ("etc/ssl/certs/ca-certificates.crt", "/etc/ssl/certs/ca-certificates.crt")];
    let wanted: Vec<_> = files.iter().map(|(from, to)| (root.join(from), Path::new(to))).filter(|(from, _)| from.exists()).collect();
    if wanted.is_empty() || !private_namespace(root) {
        return;
    }
    for (source, target) in wanted {
        bind(&source, target);
    }
}

fn main() {
    let root = runtime_root();
    bind_runtime_files(&root);
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
