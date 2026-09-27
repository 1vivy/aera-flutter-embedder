//! `aera-host-sim`: plays AERA's side of the browser bridge on a PC.
//!
//! It creates the same sealed frame memory and `SOCK_SEQPACKET` socket that
//! `aeraui/features/browser/launcher.cpp` creates, starts the worker with
//! them on fds 3 and 4, acknowledges frames like the browser scene, writes
//! chosen frames as PNG files and can replay scripted touches and keys.
//!
//! ```text
//! aera-host-sim --worker target/release/aera-browser-worker --root payload/ \
//!     --out frames/ --until 3000 --tap 316,655@1000 --key h@2000 --save-at 2500
//! ```
//!
//! Times are milliseconds after the first frame; taps use AERA's 360x700 view
//! pixels.
//! ```text
//! ```

use std::fs::File;
use std::io::BufWriter;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use aera_flutter_embedder::bridge::{self, kind, Control, Frames, Packet};

enum Input {
    Tap(i32, i32),
    Key(u32),
    Back,
}

struct Options {
    worker: PathBuf,
    root: PathBuf,
    out: PathBuf,
    /// Stop this many milliseconds after the first frame.
    until: u64,
    timeout: Duration,
    /// Save the newest frame at these times (ms after the first frame).
    save: Vec<u64>,
    script: Vec<(u64, Input)>,
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        worker: PathBuf::from("aera-browser-worker"),
        root: PathBuf::from("."),
        out: PathBuf::from("frames"),
        until: 3000,
        timeout: Duration::from_secs(60),
        save: Vec::new(),
        script: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        let at = |text: &str| -> Result<(String, u64), String> {
            let (what, ms) = text.split_once('@').ok_or("expected VALUE@MILLISECONDS")?;
            Ok((what.to_owned(), ms.parse().map_err(|_| "bad time")?))
        };
        match flag.as_str() {
            "--worker" => options.worker = value()?.into(),
            "--root" => options.root = value()?.into(),
            "--out" => options.out = value()?.into(),
            "--until" => options.until = value()?.parse().map_err(|_| "bad --until")?,
            "--timeout" => options.timeout = Duration::from_secs(value()?.parse().map_err(|_| "bad --timeout")?),
            "--save-at" => options.save.push(value()?.parse().map_err(|_| "bad --save-at")?),
            "--tap" => {
                let (point, frame) = at(&value()?)?;
                let (x, y) = point.split_once(',').ok_or("tap needs X,Y in 360x700 view pixels")?;
                let (x, y) = (x.parse().map_err(|_| "bad x")?, y.parse().map_err(|_| "bad y")?);
                options.script.push((frame, Input::Tap(x, y)));
            }
            "--key" => {
                let (key, frame) = at(&value()?)?;
                let code = match key.as_str() {
                    "enter" => 13,
                    "backspace" => 8,
                    other => other.chars().next().ok_or("empty key")? as u32,
                };
                options.script.push((frame, Input::Key(code)));
            }
            "--back" => options.script.push((value()?.parse().map_err(|_| "bad --back")?, Input::Back)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(options)
}

fn frame_memory() -> std::io::Result<OwnedFd> {
    let name = c"aera-browser-pixels";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), bridge::SHARED_BYTES as i64) } != 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

fn save_png(path: &PathBuf, bgra: &[u8]) -> std::io::Result<()> {
    let mut encoder = png::Encoder::new(BufWriter::new(File::create(path)?), bridge::WIDTH as u32, bridge::HEIGHT as u32);
    encoder.set_color(png::ColorType::Rgba);
    let mut rgba = bgra.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    encoder.write_header()?.write_image_data(&rgba)?;
    Ok(())
}

fn main() {
    let options = match parse() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("aera-host-sim: {error}");
            std::process::exit(2);
        }
    };
    std::fs::create_dir_all(&options.out).expect("create output directory");
    let memory = frame_memory().expect("frame memory");
    let frames = Frames::map(memory.as_raw_fd()).expect("map frame memory");
    // AERA uses SOCK_SEQPACKET, which std cannot create directly.
    let mut pair = [0; 2];
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, pair.as_mut_ptr()) }, 0);
    let host = Control::from_socket(unsafe { UnixDatagram::from_raw_fd(pair[0]) });
    let worker_socket = unsafe { OwnedFd::from_raw_fd(pair[1]) };

    let (memory_fd, socket_fd) = (memory.as_raw_fd(), worker_socket.as_raw_fd());
    let mut child = unsafe {
        Command::new(&options.worker)
            .arg("--isolated-ipc-v1")
            .env("AERA_FLUTTER_ROOT", &options.root)
            .pre_exec(move || {
                // Move both out of the way first: either may already sit on
                // 3 or 4, and dup2 onto itself would keep FD_CLOEXEC.
                let memory = libc::fcntl(memory_fd, libc::F_DUPFD, 10);
                let socket = libc::fcntl(socket_fd, libc::F_DUPFD, 10);
                if memory < 0 || socket < 0 || libc::dup2(memory, 3) < 0 || libc::dup2(socket, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(memory);
                libc::close(socket);
                Ok(())
            })
            .spawn()
            .expect("start worker")
    };
    drop(worker_socket);

    let started = Instant::now();
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
        save_png(&path, frames.slot(last)).expect("write png");
        println!("saved {} (frame {last})", path.display());
    };
    loop {
        let elapsed = first.map(|t| t.elapsed().as_millis() as u64);
        if let Some(ms) = elapsed {
            while script.peek().is_some_and(|(at, _)| *at <= ms) {
                let (_, input) = script.next().unwrap();
                let send = |kind: u32, x: i32, y: i32, value: u32| {
                    host.send(&Packet { kind, x, y, value, ..Packet::new(kind) }).expect("send input");
                };
                match input {
                    Input::Tap(x, y) => {
                        send(kind::TOUCH_DOWN, x, y, 0);
                        std::thread::sleep(Duration::from_millis(60));
                        send(kind::TOUCH_UP, x, y, 0);
                    }
                    Input::Key(code) => send(kind::KEY, 0, 0, code),
                    Input::Back => send(kind::BACK, 0, 0, 0),
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
        let packet = match host.recv(Some(Duration::from_millis(20))) {
            Ok(Some(packet)) => packet,
            Ok(None) => continue,
            Err(error) => {
                eprintln!("aera-host-sim: worker closed the bridge: {error}");
                exit = 1;
                break;
            }
        };
        if !packet.valid_from_worker() {
            eprintln!("aera-host-sim: AERA would reject packet kind {}", packet.kind);
            exit = 1;
            break;
        }
        match packet.kind {
            kind::FRAME => {
                if packet.sequence != last.wrapping_add(1) {
                    eprintln!("aera-host-sim: frame {} out of order after {last}", packet.sequence);
                    exit = 1;
                    break;
                }
                last = packet.sequence;
                if first.is_none() {
                    first = Some(Instant::now());
                    println!("first frame after {:.2}s", started.elapsed().as_secs_f64());
                }
                let mut ack = Packet::new(kind::ACK);
                ack.sequence = last;
                host.send(&ack).expect("ack");
            }
            kind::KEYBOARD_SHOW => println!("keyboard shown (purpose {})", packet.value),
            kind::KEYBOARD_HIDE => println!("keyboard hidden"),
            kind::STATUS | kind::ERROR => println!("{}: {}", if packet.kind == kind::ERROR { "error" } else { "status" }, packet.text),
            _ => {}
        }
    }
    if last > 0 {
        save("last".into(), last);
    }
    println!("{last} frames in {:.2}s", started.elapsed().as_secs_f64());
    if last == 0 {
        exit = 1;
    }
    let _ = host.send(&Packet::new(kind::CLOSE));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                println!("worker exited: {status}");
                break;
            }
            _ if Instant::now() > deadline => {
                let _ = child.kill();
                eprintln!("aera-host-sim: worker ignored CLOSE");
                exit = 1;
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    std::process::exit(exit);
}
