//! `aera-host-sim`: plays AERA's generic pixel host on a PC.
//!
//! It speaks the host side of the assumed Host API 3 (see `host.rs`): creates
//! a sealed surface memfd and a `SOCK_SEQPACKET` channel, starts the plugin
//! with them on fds 3 and 4, answers the handshake, frees each presented slot
//! like AERA would, writes chosen frames as PNG files and replays scripted
//! touches, keys and Back.
//!
//! ```text
//! aera-host-sim --plugin payload/usr/bin/aera-plugin --root payload/ \
//!     --out frames/ --until 3000 --tap 105,218@1000 --key h@2000 --save-at 2500
//!     [--drag 100,300>200,400@1500] [--ack-delay 12] [--slots 2]
//! ```
//!
//! Times are milliseconds after the first frame. Taps are in logical pixels
//! (surface pixels divided by `--scale`).

use std::fs::File;
use std::io::BufWriter;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use aera_flutter_embedder::host::{self, kind, lifecycle, Control, Geometry, Message, Surface};

enum Input {
    Tap(f64, f64),
    /// Press at the first point, move to the second over ~300 ms, release.
    Drag(f64, f64, f64, f64),
    Key(u32),
    Back,
    Pause,
    Resume,
}

struct Options {
    plugin: PathBuf,
    root: PathBuf,
    out: PathBuf,
    geometry: Geometry,
    /// Stop this many milliseconds after the first frame.
    until: u64,
    timeout: Duration,
    /// Save the newest frame at these times (ms after the first frame).
    save: Vec<u64>,
    script: Vec<(u64, Input)>,
    /// Hold each FRAME_DONE this long, like AERA waiting for its display refresh.
    ack_delay: Duration,
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        plugin: PathBuf::from("aera-plugin"),
        root: PathBuf::from("."),
        out: PathBuf::from("frames"),
        geometry: Geometry::PHONE,
        until: 3000,
        timeout: Duration::from_secs(60),
        save: Vec::new(),
        script: Vec::new(),
        ack_delay: Duration::ZERO,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        let at = |text: &str| -> Result<(String, u64), String> {
            let (what, ms) = text.split_once('@').ok_or("expected VALUE@MILLISECONDS")?;
            Ok((what.to_owned(), ms.parse().map_err(|_| "bad time")?))
        };
        let ms = |text: String| -> Result<u64, String> { text.parse().map_err(|_| format!("bad time for {flag}")) };
        match flag.as_str() {
            "--plugin" => options.plugin = value()?.into(),
            "--root" => options.root = value()?.into(),
            "--out" => options.out = value()?.into(),
            "--size" => {
                let text = value()?;
                let (w, h) = text.split_once('x').ok_or("--size needs WIDTHxHEIGHT")?;
                options.geometry.width = w.parse().map_err(|_| "bad width")?;
                options.geometry.height = h.parse().map_err(|_| "bad height")?;
                options.geometry.stride = options.geometry.width * 4;
            }
            "--slots" => options.geometry.slots = value()?.parse().map_err(|_| "bad --slots")?,
            "--ack-delay" => options.ack_delay = Duration::from_millis(value()?.parse().map_err(|_| "bad --ack-delay")?),
            "--drag" => {
                let (path, time) = at(&value()?)?;
                let numbers: Vec<f64> = path
                    .split([',', '>'])
                    .map(|n| n.parse().map_err(|_| "drag needs X1,Y1>X2,Y2"))
                    .collect::<Result<_, _>>()?;
                let [x1, y1, x2, y2] = numbers[..] else { return Err("drag needs X1,Y1>X2,Y2".into()) };
                options.script.push((time, Input::Drag(x1, y1, x2, y2)));
            }
            "--scale" => options.geometry.scale = value()?.parse().map_err(|_| "bad --scale")?,
            "--until" => options.until = ms(value()?)?,
            "--timeout" => options.timeout = Duration::from_secs(value()?.parse().map_err(|_| "bad --timeout")?),
            "--save-at" => options.save.push(ms(value()?)?),
            "--tap" => {
                let (point, time) = at(&value()?)?;
                let (x, y) = point.split_once(',').ok_or("tap needs X,Y in logical pixels")?;
                let (x, y) = (x.parse().map_err(|_| "bad x")?, y.parse().map_err(|_| "bad y")?);
                options.script.push((time, Input::Tap(x, y)));
            }
            "--key" => {
                let (key, time) = at(&value()?)?;
                let code = match key.as_str() {
                    "enter" => 13,
                    "backspace" => 8,
                    other => other.chars().next().ok_or("empty key")? as u32,
                };
                options.script.push((time, Input::Key(code)));
            }
            "--back" => options.script.push((ms(value()?)?, Input::Back)),
            "--pause" => options.script.push((ms(value()?)?, Input::Pause)),
            "--resume" => options.script.push((ms(value()?)?, Input::Resume)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if !options.geometry.valid() {
        return Err("unusable --size or --scale".into());
    }
    Ok(options)
}

fn surface_memory(bytes: usize) -> std::io::Result<OwnedFd> {
    let name = c"aera-plugin-surface";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), bytes as i64) } != 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

fn save_png(path: &PathBuf, geometry: Geometry, bgra: &[u8]) -> std::io::Result<()> {
    let mut encoder = png::Encoder::new(BufWriter::new(File::create(path)?), geometry.width, geometry.height);
    encoder.set_color(png::ColorType::Rgba);
    let row = geometry.width as usize * 4;
    let mut rgba = Vec::with_capacity(row * geometry.height as usize);
    for line in bgra.chunks_exact(geometry.stride as usize) {
        rgba.extend_from_slice(&line[..row]);
    }
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    encoder.write_header()?.write_image_data(&rgba)?;
    Ok(())
}

fn fail(error: &str) -> ! {
    eprintln!("aera-host-sim: {error}");
    std::process::exit(1);
}

fn main() {
    let options = match parse() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("aera-host-sim: {error}");
            std::process::exit(2);
        }
    };
    let geometry = options.geometry;
    std::fs::create_dir_all(&options.out).expect("create output directory");
    let memory = surface_memory(geometry.total_bytes()).expect("surface memory");
    let surface = Surface::map(memory.as_raw_fd(), geometry).expect("map surface memory");
    // std cannot create SOCK_SEQPACKET pairs directly.
    let mut pair = [0; 2];
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, pair.as_mut_ptr()) }, 0);
    let host = Control::from_socket(unsafe { UnixDatagram::from_raw_fd(pair[0]) });
    let plugin_socket = unsafe { OwnedFd::from_raw_fd(pair[1]) };

    let (memory_fd, socket_fd) = (memory.as_raw_fd(), plugin_socket.as_raw_fd());
    let mut child = unsafe {
        Command::new(&options.plugin)
            .arg(format!("--aera-host-api={}", host::HOST_API))
            .env(host::ROOT_ENV, &options.root)
            .env(host::CONTROL_FD_ENV, host::CONTROL_FD.to_string())
            .env(host::SURFACE_FD_ENV, host::SURFACE_FD.to_string())
            .env("AERA_HOST_API", host::HOST_API.to_string())
            .pre_exec(move || {
                // Move both out of the way first: either may already sit on
                // 3 or 4, and dup2 onto itself would keep FD_CLOEXEC.
                let memory = libc::fcntl(memory_fd, libc::F_DUPFD, 10);
                let socket = libc::fcntl(socket_fd, libc::F_DUPFD, 10);
                if memory < 0
                    || socket < 0
                    || libc::dup2(memory, host::SURFACE_FD) < 0
                    || libc::dup2(socket, host::CONTROL_FD) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(memory);
                libc::close(socket);
                Ok(())
            })
            .spawn()
            .expect("start plugin")
    };
    drop(plugin_socket);

    let started = Instant::now();
    let mut negotiated = false;
    let mut first: Option<Instant> = None;
    let mut script = options.script;
    script.sort_by_key(|(ms, _)| *ms);
    let mut script = script.into_iter().peekable();
    let mut saves = options.save.clone();
    saves.sort();
    let mut saves = saves.into_iter().peekable();
    let mut last = 0u32;
    let mut exit = 0;
    let save = |label: String, last: u32| {
        let path = options.out.join(format!("{label}.png"));
        save_png(&path, geometry, surface.slot(last)).expect("write png");
        println!("saved {} (frame {last})", path.display());
    };
    let send = |message: Message| host.send(&message).unwrap_or_else(|e| fail(&format!("send: {e}")));
    loop {
        if let Some(ms) = first.map(|t| t.elapsed().as_millis() as u64) {
            while script.peek().is_some_and(|(at, _)| *at <= ms) {
                let (_, input) = script.next().unwrap();
                match input {
                    Input::Tap(x, y) => {
                        let (x, y) = ((x * geometry.scale) as u32, (y * geometry.scale) as u32);
                        send(Message { value: x, flags: y, ..Message::new(kind::TOUCH_DOWN) });
                        std::thread::sleep(Duration::from_millis(60));
                        send(Message { value: x, flags: y, ..Message::new(kind::TOUCH_UP) });
                    }
                    Input::Drag(x1, y1, x2, y2) => {
                        let at = |x: f64, y: f64, kind: u32| Message {
                            value: (x * geometry.scale) as u32,
                            flags: (y * geometry.scale) as u32,
                            ..Message::new(kind)
                        };
                        send(at(x1, y1, kind::TOUCH_DOWN));
                        for step in 1..=10 {
                            std::thread::sleep(Duration::from_millis(30));
                            let t = step as f64 / 10.0;
                            send(at(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t, kind::TOUCH_MOVE));
                        }
                        send(at(x2, y2, kind::TOUCH_UP));
                    }
                    Input::Key(code) => send(Message { value: code, ..Message::new(kind::KEY) }),
                    Input::Back => send(Message::new(kind::BACK)),
                    Input::Pause => send(Message { value: lifecycle::PAUSE, ..Message::new(kind::LIFECYCLE) }),
                    Input::Resume => send(Message { value: lifecycle::RESUME, ..Message::new(kind::LIFECYCLE) }),
                }
            }
            while saves.peek().is_some_and(|at| *at <= ms) {
                let at = saves.next().unwrap();
                save(format!("at-{at:05}ms"), last);
            }
            if ms >= options.until {
                break;
            }
        }
        if started.elapsed() > options.timeout {
            eprintln!("aera-host-sim: timed out");
            exit = 1;
            break;
        }
        let message = match host.recv(Some(Duration::from_millis(20))) {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(error) => {
                eprintln!("aera-host-sim: plugin closed the channel: {error}");
                exit = 1;
                break;
            }
        };
        if !kind::from_worker(message.kind) {
            eprintln!("aera-host-sim: AERA would reject message kind {}", message.kind);
            exit = 1;
            break;
        }
        if !negotiated {
            if message.kind != kind::HELLO || message.value > host::PROTOCOL_VERSION || message.flags < host::PROTOCOL_VERSION {
                eprintln!("aera-host-sim: plugin did not open with a usable HELLO");
                exit = 1;
                break;
            }
            negotiated = true;
            send(Message {
                value: host::PROTOCOL_VERSION,
                flags: host::FEATURE_METRICS | host::FEATURE_BACK_NAVIGATION | host::FEATURE_PIXEL_SURFACE,
                ..Message::new(kind::HELLO_ACK)
            });
            send(geometry.to_message());
            send(Message { value: lifecycle::RESUME, ..Message::new(kind::LIFECYCLE) });
            continue;
        }
        match message.kind {
            kind::PRESENT => {
                if message.request_id != last.wrapping_add(1) || message.value != surface.slot_of(message.request_id) {
                    eprintln!("aera-host-sim: frame {} in slot {} after {last}", message.request_id, message.value);
                    exit = 1;
                    break;
                }
                last = message.request_id;
                if first.is_none() {
                    first = Some(Instant::now());
                    println!("first frame after {:.2}s", started.elapsed().as_secs_f64());
                }
                std::thread::sleep(options.ack_delay);
                send(Message { request_id: last, ..Message::new(kind::FRAME_DONE) });
            }
            kind::KEYBOARD_SHOW => println!("keyboard shown (purpose {})", message.value),
            kind::KEYBOARD_HIDE => println!("keyboard hidden"),
            kind::SET_STATUS => println!("status: {}", message.text),
            kind::CLOSE => {
                println!("plugin asked to close");
                break;
            }
            other => println!("ignored message kind {other}"),
        }
    }
    if last > 0 {
        save("last".into(), last);
    }
    println!("{last} frames in {:.2}s", started.elapsed().as_secs_f64());
    if last == 0 {
        exit = 1;
    }
    let _ = host.send(&Message { value: lifecycle::STOP, ..Message::new(kind::LIFECYCLE) });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                println!("plugin exited: {status}");
                break;
            }
            _ if Instant::now() > deadline => {
                let _ = child.kill();
                eprintln!("aera-host-sim: plugin ignored LIFECYCLE stop");
                exit = 1;
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    std::process::exit(exit);
}
