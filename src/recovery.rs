//! Recovery itself, for Flutter apps on AERA's generic plugin host.
//!
//! Generic plugins are recovery modules: they run as root in recovery's own
//! namespaces (`aeraui/features/plugin_api/README.md`), so the embedder can
//! give apps what AERA's own screens use, the way Flutter's Android shell
//! gives apps the system's settings:
//!
//! - `flutter/settings`: AERA's light or dark theme, interface size and 24-hour
//!   clock, from AERA's preferences file.
//! - `flutter/platform`: `HapticFeedback` on the phone's vibrator, honouring
//!   AERA's haptic settings, and a `Clipboard` shared by every Flutter app.
//! - Time zone: AERA's, when recovery did not pass `TZ` on.
//! - `aera/recovery`: AERA's accent colour, recovery and device facts,
//!   battery, screen brightness, flashlight, Wi-Fi and reboot.
//!
//! Paths follow AERA-Recovery/android_bootable_recovery
//! (`aeraui/platform/aera_backend.cpp`, `minuitwrp/events.cpp`) and the dodge
//! device tree. `AERA_RECOVERY_ROOT` moves every path under another directory
//! so the simulator can stand in a fake phone.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{json, Value};

pub const CHANNEL: &str = "aera/recovery";

fn path(p: &str) -> PathBuf {
    match std::env::var_os("AERA_RECOVERY_ROOT") {
        Some(root) => PathBuf::from(root).join(p.trim_start_matches('/')),
        None => PathBuf::from(p),
    }
}

fn read(p: &str) -> Option<String> {
    fs::read_to_string(path(p)).ok().map(|s| s.trim().to_owned())
}

fn read_i64(p: &str) -> Option<i64> {
    read(p)?.parse().ok()
}

/// AERA's saved preferences (`key=value` lines). Empty before `/data` is
/// decrypted, like in AERA itself.
pub fn preferences() -> HashMap<String, String> {
    ["/data/media/0/AERA/preferences.conf", "/sdcard/AERA/preferences.conf"]
        .iter()
        .find_map(|p| fs::read_to_string(path(p)).ok())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// What Flutter reads from `flutter/settings`.
pub fn settings(prefs: &HashMap<String, String>) -> Value {
    json!({
        "textScaleFactor": text_scale(prefs),
        "alwaysUse24HourFormat": prefs.get("clock24").is_some_and(|v| v == "1"),
        "platformBrightness": if light(prefs) { "light" } else { "dark" },
    })
}

fn light(prefs: &HashMap<String, String>) -> bool {
    prefs.get("theme").is_some_and(|t| t == "light")
}

/// AERA's interface size: 0 small, 1 normal (also when unset), 2 large.
fn interface_size(prefs: &HashMap<String, String>) -> &'static str {
    match prefs.get("interface_size").map(String::as_str) {
        Some("0") => "small",
        Some("2") => "large",
        _ => "normal",
    }
}

fn text_scale(prefs: &HashMap<String, String>) -> f64 {
    match interface_size(prefs) {
        "small" => 0.9,
        "large" => 1.15,
        _ => 1.0,
    }
}

/// AERA's accent colour as 0xRRGGBB (its default is 0x16c8ff).
fn accent(prefs: &HashMap<String, String>) -> u32 {
    prefs
        .get("accent")
        .and_then(|v| u32::from_str_radix(v, 16).ok())
        .filter(|&v| v != 0 && v <= 0xff_ffff)
        .unwrap_or(0x16c8ff)
}

/// Sets `TZ` from AERA's time zone setting when recovery did not pass one
/// on. Call before the engine starts; Dart reads local time through libc.
pub fn apply_time_zone() {
    if std::env::var_os("TZ").is_some_and(|tz| !tz.is_empty()) {
        return;
    }
    if let Some(zone) = preferences().get("timezone").filter(|z| !z.is_empty()) {
        std::env::set_var("TZ", zone);
    }
}

fn property(name: &str) -> String {
    Command::new("getprop")
        .arg(name)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Runs a `flutter/platform` method this module owns. `None` when it is not one.
pub fn platform(method: &str, args: &Value) -> Option<Value> {
    match method {
        "HapticFeedback.vibrate" => {
            let prefs = preferences();
            // AERA's own touch, keyboard and action pulse lengths, where 0 is off.
            let key = match args.as_str() {
                Some("HapticFeedbackType.heavyImpact") | Some("HapticFeedbackType.mediumImpact") | None => "haptic_action",
                Some("HapticFeedbackType.selectionClick") => "haptic_keyboard",
                _ => "haptic_touch",
            };
            let milliseconds = prefs.get(key).and_then(|v| v.parse().ok()).unwrap_or(if key == "haptic_action" { 40 } else { 15 });
            if milliseconds > 0 {
                let _ = vibrate(milliseconds);
            }
            Some(json!([null]))
        }
        "Clipboard.setData" => {
            let text = args["text"].as_str().unwrap_or_default();
            let _ = fs::write(path(CLIPBOARD), text);
            Some(json!([null]))
        }
        "Clipboard.getData" => Some(match fs::read_to_string(path(CLIPBOARD)) {
            Ok(text) => json!([{"text": text}]),
            Err(_) => json!([null]),
        }),
        "Clipboard.hasStrings" => {
            let has = fs::metadata(path(CLIPBOARD)).is_ok_and(|m| m.len() > 0);
            Some(json!([{"value": has}]))
        }
        _ => None,
    }
}

/// One clipboard for every Flutter app in this recovery session.
const CLIPBOARD: &str = "/tmp/aera-flutter-clipboard";

/// Runs an `aera/recovery` method. Replies in the JSON method codec's
/// envelope: `[result]`, or `["code", "message", null]` on failure.
pub fn call(method: &str, args: &Value) -> Value {
    let result = match method {
        "getTheme" => {
            let prefs = preferences();
            Ok(json!({
                "accent": accent(&prefs),
                "light": light(&prefs),
                "interfaceSize": interface_size(&prefs),
                "clock24": prefs.get("clock24").is_some_and(|v| v == "1"),
            }))
        }
        "getInfo" => Ok(json!({
            "version": property("ro.aera.release.version"),
            "channel": property("ro.aera.release.channel"),
            "status": property("ro.aera.build.status"),
            "device": property("ro.product.model"),
            "slot": property("ro.boot.slot_suffix").trim_start_matches('_').to_uppercase(),
        })),
        "getBattery" => battery(),
        "getBrightness" => brightness().map(|percent| json!(percent)),
        "setBrightness" => set_brightness(args.as_i64().unwrap_or(-1)).map(|_| Value::Null),
        "hasFlashlight" => Ok(json!(flashlight_dirs().next().is_some())),
        "setFlashlight" => set_flashlight(args.as_bool().unwrap_or(false)).map(|_| Value::Null),
        "getWifi" => Ok(wifi()),
        "vibrate" => vibrate(args.as_u64().unwrap_or(30).min(5000) as u32).map(|_| Value::Null),
        "reboot" => reboot(args.as_str().unwrap_or("system")).map(|_| Value::Null),
        _ => return Value::Null,
    };
    match result {
        Ok(value) => json!([value]),
        Err(message) => json!(["error", message, null]),
    }
}

fn battery() -> Result<Value, String> {
    let dir = ["/sys/class/power_supply/battery", "/sys/class/power_supply/bms"]
        .into_iter()
        .find(|d| path(&format!("{d}/capacity")).exists())
        .ok_or("no battery found")?;
    let get = |name: &str| read(&format!("{dir}/{name}"));
    let number = |name: &str| read_i64(&format!("{dir}/{name}"));
    Ok(json!({
        "level": number("capacity"),
        "status": get("status"),
        "health": get("health"),
        // The kernel reports tenths of a degree, microvolts and microamps.
        "temperatureC": number("temp").map(|t| t as f64 / 10.0),
        "voltageMv": number("voltage_now").map(|v| v / 1000),
        "currentMa": number("current_now").map(|c| c / 1000),
    }))
}

fn backlight() -> Result<PathBuf, String> {
    let base = path("/sys/class/backlight");
    let mut entries: Vec<_> = fs::read_dir(&base).map_err(|_| "no backlight")?.flatten().map(|e| e.path()).collect();
    entries.sort();
    // dodge: panel0-backlight (AERA_BRIGHTNESS_PATH).
    entries.into_iter().find(|p| p.join("max_brightness").exists()).ok_or_else(|| "no backlight".into())
}

fn brightness() -> Result<i64, String> {
    let dir = backlight()?;
    let get = |name| fs::read_to_string(dir.join(name)).ok().and_then(|s| s.trim().parse::<i64>().ok());
    let (now, max) = (get("brightness").ok_or("unreadable")?, get("max_brightness").ok_or("unreadable")?);
    Ok(if max > 0 { (now * 100 + max / 2) / max } else { 0 })
}

/// Sets the backlight in percent, clamped to 10..100 like AERA's slider.
/// AERA's own setting is left alone, so AERA restores it on its screens.
fn set_brightness(percent: i64) -> Result<(), String> {
    if percent < 0 {
        return Err("brightness must be a percentage".into());
    }
    let dir = backlight()?;
    let max: i64 = fs::read_to_string(dir.join("max_brightness")).ok().and_then(|s| s.trim().parse().ok()).ok_or("unreadable")?;
    let value = max * percent.clamp(10, 100) / 100;
    fs::write(dir.join("brightness"), value.to_string()).map_err(|e| e.to_string())
}

/// Torch LEDs, as AERA looks for them.
fn flashlight_dirs() -> impl Iterator<Item = PathBuf> {
    let leds = path("/sys/class/leds");
    let mut found: Vec<PathBuf> = fs::read_dir(&leds)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            (name.contains("torch") || name == "flashlight") && p.join("brightness").exists()
        })
        .collect();
    found.sort();
    found.into_iter()
}

fn set_flashlight(on: bool) -> Result<(), String> {
    let mut any = false;
    for dir in flashlight_dirs() {
        let value = if on {
            fs::read_to_string(dir.join("max_brightness")).ok().map(|s| s.trim().to_owned()).unwrap_or_else(|| "255".into())
        } else {
            "0".into()
        };
        any |= fs::write(dir.join("brightness"), value).is_ok();
    }
    if any { Ok(()) } else { Err("no flashlight".into()) }
}

fn wifi() -> Value {
    let interface = "wlan0";
    let up = read(&format!("/sys/class/net/{interface}/operstate")).is_some_and(|s| s == "up");
    let mut ssid = None;
    let mut address = None;
    if up {
        if let Ok(out) = Command::new("wpa_cli").args(["-i", interface, "status"]).output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(v) = line.strip_prefix("ssid=") {
                    ssid = Some(v.to_owned());
                } else if let Some(v) = line.strip_prefix("ip_address=") {
                    address = Some(v.to_owned());
                }
            }
        }
    }
    json!({"interface": interface, "connected": up, "ssid": ssid, "ipAddress": address})
}

/// Reboots through init, as recovery's own reboot menu does.
fn reboot(target: &str) -> Result<(), String> {
    let command = match target {
        "system" => "reboot,",
        "recovery" => "reboot,recovery",
        "bootloader" => "reboot,bootloader",
        "fastboot" => "reboot,fastboot",
        "poweroff" => "shutdown,",
        _ => return Err(format!("unknown reboot target {target}")),
    };
    let status = Command::new("setprop").args(["sys.powerctl", command]).status().map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("setprop failed ({status})")) }
}

/// Runs the vibrator for `milliseconds`. The dodge vibrator is an input
/// force-feedback device (AERA uses the AIDL haptics HAL over it), so this
/// plays a constant effect through evdev, and falls back to the older LED
/// class vibrator.
pub fn vibrate(milliseconds: u32) -> Result<(), String> {
    let milliseconds = milliseconds.clamp(1, 5000);
    if ff::play(milliseconds).is_ok() {
        return Ok(());
    }
    let dir = path("/sys/class/leds/vibrator");
    if dir.join("activate").exists() {
        fs::write(dir.join("duration"), milliseconds.to_string()).map_err(|e| e.to_string())?;
        return fs::write(dir.join("activate"), "1").map_err(|e| e.to_string());
    }
    Err("no vibrator found".into())
}

mod ff {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    const EV_FF: u16 = 0x15;
    const FF_RUMBLE: u16 = 0x50;
    const FF_PERIODIC: u16 = 0x51;
    const FF_CONSTANT: u16 = 0x52;
    const FF_SINE: u16 = 0x5a;
    const FF_MAX: usize = 0x7f;

    /// `struct ff_effect` on 64-bit Linux: 48 bytes, the union at 16.
    #[repr(C)]
    struct Effect {
        kind: u16,
        id: i16,
        direction: u16,
        trigger: [u16; 2],
        replay_length: u16,
        replay_delay: u16,
        union: [u64; 4],
    }

    #[repr(C)]
    struct InputEvent {
        time: [i64; 2],
        kind: u16,
        code: u16,
        value: i32,
    }

    const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
        (dir << 30) | (size << 16) | ((b'E' as u64) << 8) | nr
    }

    pub fn play(milliseconds: u32) -> Result<(), ()> {
        let dir = super::path("/dev/input");
        let mut devices: Vec<_> = std::fs::read_dir(dir).map_err(|_| ())?.flatten().map(|e| e.path()).collect();
        devices.sort();
        for device in devices {
            if !device.file_name().is_some_and(|n| n.to_string_lossy().starts_with("event")) {
                continue;
            }
            let Ok(mut file) = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(&device) else { continue };
            let fd = file.as_raw_fd();
            let mut bits = [0u8; FF_MAX / 8 + 1];
            let get_bits = ioc(2, 0x20 + EV_FF as u64, bits.len() as u64);
            if unsafe { libc::ioctl(fd, get_bits as _, bits.as_mut_ptr()) } < 0 {
                continue;
            }
            let has = |bit: u16| bits[bit as usize / 8] & (1 << (bit % 8)) != 0;
            let mut effect = Effect {
                kind: 0,
                id: -1,
                direction: 0,
                trigger: [0; 2],
                replay_length: milliseconds.min(u16::MAX as u32) as u16,
                replay_delay: 0,
                union: [0; 4],
            };
            let words = |values: &[u16]| {
                let mut out = [0u64; 4];
                let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
                for (i, chunk) in bytes.chunks(8).enumerate() {
                    let mut word = [0u8; 8];
                    word[..chunk.len()].copy_from_slice(chunk);
                    out[i] = u64::from_le_bytes(word);
                }
                out
            };
            if has(FF_CONSTANT) {
                effect.kind = FF_CONSTANT;
                effect.union = words(&[0x7fff]); // level
            } else if has(FF_RUMBLE) {
                effect.kind = FF_RUMBLE;
                effect.union = words(&[0xffff, 0xffff]); // strong, weak
            } else if has(FF_PERIODIC) {
                effect.kind = FF_PERIODIC;
                effect.union = words(&[FF_SINE, 5, 0x7fff]); // waveform, period ms, magnitude
            } else {
                continue;
            }
            let upload = ioc(1, 0x80, std::mem::size_of::<Effect>() as u64);
            if unsafe { libc::ioctl(fd, upload as _, &mut effect) } < 0 {
                continue;
            }
            let start = InputEvent { time: [0; 2], kind: EV_FF, code: effect.id as u16, value: 1 };
            let bytes = unsafe {
                std::slice::from_raw_parts((&start as *const InputEvent).cast::<u8>(), std::mem::size_of::<InputEvent>())
            };
            if file.write_all(bytes).is_ok() {
                // Closing the device erases the effect, so keep it open until
                // the pulse is over.
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(milliseconds as u64 + 20));
                    let remove = ioc(1, 0x81, std::mem::size_of::<libc::c_int>() as u64);
                    unsafe { libc::ioctl(file.as_raw_fd(), remove as _, effect.id as libc::c_int) };
                    drop(file);
                });
                return Ok(());
            }
        }
        Err(())
    }

    #[test]
    fn struct_sizes_match_the_kernel() {
        assert_eq!(std::mem::size_of::<Effect>(), 48);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn settings_follow_aera() {
        let p = prefs(&[("theme", "light"), ("interface_size", "2"), ("clock24", "1")]);
        assert_eq!(settings(&p), json!({"textScaleFactor": 1.15, "alwaysUse24HourFormat": true, "platformBrightness": "light"}));
        let defaults = settings(&HashMap::new());
        assert_eq!(defaults["platformBrightness"], "dark");
        assert_eq!(defaults["textScaleFactor"], 1.0);
    }

    #[test]
    fn accent_falls_back_to_aera_default() {
        assert_eq!(accent(&prefs(&[("accent", "ff8800")])), 0xff8800);
        assert_eq!(accent(&prefs(&[("accent", "nope")])), 0x16c8ff);
    }

    #[test]
    fn reads_and_writes_a_fake_phone() {
        let root = std::env::temp_dir().join(format!("aera-recovery-test-{}", std::process::id()));
        let w = |p: &str, v: &str| {
            let f = root.join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, v).unwrap();
        };
        w("sys/class/power_supply/battery/capacity", "87\n");
        w("sys/class/power_supply/battery/temp", "312");
        w("sys/class/power_supply/battery/status", "Charging");
        w("sys/class/backlight/panel0-backlight/max_brightness", "4095");
        w("sys/class/backlight/panel0-backlight/brightness", "2048");
        w("sys/class/leds/led:torch_0/brightness", "0");
        w("sys/class/leds/led:torch_0/max_brightness", "500");
        w("data/media/0/AERA/preferences.conf", "accent=ff8800\ntheme=light\n");
        w("tmp/.keep", "");
        std::env::set_var("AERA_RECOVERY_ROOT", &root);
        let battery = call("getBattery", &Value::Null);
        assert_eq!(battery[0]["level"], 87);
        assert_eq!(battery[0]["temperatureC"], 31.2);
        assert_eq!(call("getBrightness", &Value::Null), json!([50]));
        assert_eq!(call("setBrightness", &json!(25)), json!([null]));
        assert_eq!(fs::read_to_string(root.join("sys/class/backlight/panel0-backlight/brightness")).unwrap(), "1023");
        assert_eq!(call("setFlashlight", &json!(true)), json!([null]));
        assert_eq!(fs::read_to_string(root.join("sys/class/leds/led:torch_0/brightness")).unwrap(), "500");
        assert_eq!(call("getTheme", &Value::Null)[0]["accent"], 0xff8800);
        assert_eq!(platform("Clipboard.getData", &Value::Null), Some(json!([null])));
        platform("Clipboard.setData", &json!({"text": "hi"}));
        assert_eq!(platform("Clipboard.getData", &Value::Null), Some(json!([{"text": "hi"}])));
        std::env::remove_var("AERA_RECOVERY_ROOT");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_methods_are_not_implemented() {
        assert_eq!(call("nope", &Value::Null), Value::Null);
        assert!(platform("SystemSound.play", &Value::Null).is_none());
        assert_eq!(call("reboot", &json!("moon"))[0], "error");
    }
}
