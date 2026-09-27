//! AERA's generic pixel + GPU plugin host, as **assumed** before it ships.
//!
//! An AERA maintainer is adding a host that gives any plugin ID a GPU and a
//! full-screen pixel surface without AERA's browser chrome. Its interface is
//! not published yet, so this file is the embedder's single place for every
//! guess about it. When the official host lands, change this file (and the
//! manifest constants at the top of `tools/make_aerap.py`) and nothing else
//! should need to move. Each guess is marked `ASSUMED`.
//!
//! What the guesses are built on (AERA-Recovery/android_bootable_recovery,
//! branch aera-16.0):
//!
//! - `aeraui/features/plugin_api/`: Host API 2. Plugins install under any ID
//!   as `type: "ui-runtime"`, `entry: "main"`, `executable:
//!   "usr/bin/aera-plugin"`. AERA starts `usr/bin/aera-plugin
//!   --aera-host-api=2` with a `SOCK_SEQPACKET` channel on fd 4
//!   (`AERA_PLUGIN_FD=4`), closes fd 3, and speaks fixed 1144-byte `A2PI`
//!   messages. `HELLO_ACK` flags advertise optional features.
//! - `aeraui/features/browser/`: the only host that draws plugin pixels on the
//!   GPU today. It shares a sealed memfd of two frame slots on fd 3 and
//!   acknowledges each frame before the worker may reuse its slot.
//!
//! So the assumed host is Host API 2 with a pixel surface bolted on:
//!
//! | | Assumed |
//! | --- | --- |
//! | Launch | `usr/bin/aera-plugin --aera-host-api=3` |
//! | Control | fd `AERA_PLUGIN_FD` (4), `A2PI` messages, protocol version 3 |
//! | Handshake | `HELLO` (3..3) → `HELLO_ACK` with [`FEATURE_PIXEL_SURFACE`] → `SURFACE` |
//! | Pixels | sealed memfd on fd `AERA_SURFACE_FD` (3), BGRA8888, top-down, `slots` frames of `stride * height` |
//! | Frames | worker sends `PRESENT` for slot `sequence % slots`, host answers `FRAME_DONE` |
//! | Input | `TOUCH_DOWN/MOVE/UP` in surface pixels, `KEY` code points, `BACK` |
//! | Lifecycle | Host API 2's `LIFECYCLE` resume, pause, stop |
//! | Process | root in recovery's own namespaces, no jail, like every Host API 2 plugin (plugins are recovery modules) |
//! | Filesystem | payload extracted like Host API 2, named in `AERA_PLUGIN_ROOT`; recovery's `/` stays `/` |
//! | GPU | `/dev/kgsl-3d0` and the DMA heap, which root already reaches; the host only has to leave the GPU free while the plugin is in front |

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};

/// `A2PI`, unchanged from Host API 2.
pub const MAGIC: u32 = 0x4132_5049;
/// ASSUMED: the pixel host is Host API 3 and bumps the wire version with it.
pub const PROTOCOL_VERSION: u32 = 3;
pub const HOST_API: u32 = 3;
pub const TITLE_LEN: usize = 96;
pub const TEXT_LEN: usize = 1024;
/// Same layout as `aeraui::plugin_api::Message`.
pub const MESSAGE_LEN: usize = 6 * 4 + TITLE_LEN + TEXT_LEN;

/// Host API 2 names the control descriptor in `AERA_PLUGIN_FD`; it is 4.
pub const CONTROL_FD: RawFd = 4;
/// ASSUMED: the surface arrives on fd 3, which Host API 2 leaves closed and
/// the browser host already uses for its frame memory.
pub const SURFACE_FD: RawFd = 3;
pub const CONTROL_FD_ENV: &str = "AERA_PLUGIN_FD";
/// ASSUMED name.
pub const SURFACE_FD_ENV: &str = "AERA_SURFACE_FD";
/// Host API 2: the extracted runtime directory.
pub const ROOT_ENV: &str = "AERA_PLUGIN_ROOT";

/// Host API 2 features, plus the assumed pixel surface bit.
pub const FEATURE_METRICS: u32 = 1 << 0;
pub const FEATURE_BACK_NAVIGATION: u32 = 1 << 1;
/// ASSUMED: the next free bit.
pub const FEATURE_PIXEL_SURFACE: u32 = 1 << 2;

/// Message kinds. Host API 2 numbers are kept; new ones are appended after
/// each direction's last kind so no existing number moves.
pub mod kind {
    // Worker to host, Host API 2.
    pub const HELLO: u32 = 1;
    pub const SET_STATUS: u32 = 5;
    pub const CLOSE: u32 = 7;
    // Worker to host, ASSUMED.
    /// A frame is ready: `request_id` sequence, `value` slot.
    pub const PRESENT: u32 = 12;
    /// Show AERA's keyboard; `value` is the input purpose (0 text, 2 digits).
    pub const KEYBOARD_SHOW: u32 = 13;
    pub const KEYBOARD_HIDE: u32 = 14;

    // Host to worker, Host API 2.
    pub const HELLO_ACK: u32 = 64;
    pub const LIFECYCLE: u32 = 66;
    // Host to worker, ASSUMED.
    /// Surface geometry: `value` width, `flags` height, `request_id` stride in
    /// bytes, `title` pixel format, `text` `slots=N scale=S`.
    pub const SURFACE: u32 = 68;
    /// The host has read the frame `request_id`; its slot is free again.
    pub const FRAME_DONE: u32 = 69;
    /// `request_id` pointer, `value` x, `flags` y, in surface pixels.
    pub const TOUCH_DOWN: u32 = 70;
    pub const TOUCH_MOVE: u32 = 71;
    pub const TOUCH_UP: u32 = 72;
    /// `value` is a Unicode code point; 8 backspace, 13 enter.
    pub const KEY: u32 = 73;
    /// AERA's edge-back gesture.
    pub const BACK: u32 = 74;

    pub fn from_worker(kind: u32) -> bool {
        (HELLO..=KEYBOARD_HIDE).contains(&kind)
    }

    pub fn from_host(kind: u32) -> bool {
        (HELLO_ACK..=BACK).contains(&kind)
    }
}

/// Host API 2 lifecycle values.
pub mod lifecycle {
    pub const RESUME: u32 = 1;
    pub const PAUSE: u32 = 2;
    pub const STOP: u32 = 3;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub kind: u32,
    pub request_id: u32,
    pub value: u32,
    pub flags: u32,
    pub title: String,
    pub text: String,
}

fn put_str(out: &mut [u8], text: &str) {
    let mut end = text.len().min(out.len() - 1);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    out[..end].copy_from_slice(&text.as_bytes()[..end]);
}

fn get_str(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

impl Message {
    pub fn new(kind: u32) -> Message {
        Message { kind, ..Default::default() }
    }

    pub fn encode(&self) -> [u8; MESSAGE_LEN] {
        let mut out = [0u8; MESSAGE_LEN];
        let words = [MAGIC, PROTOCOL_VERSION, self.kind, self.request_id, self.value, self.flags];
        for (i, word) in words.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        put_str(&mut out[24..24 + TITLE_LEN], &self.title);
        put_str(&mut out[24 + TITLE_LEN..], &self.text);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Message> {
        if bytes.len() != MESSAGE_LEN {
            return None;
        }
        let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        if word(0) != MAGIC || word(1) != PROTOCOL_VERSION {
            return None;
        }
        Some(Message {
            kind: word(2),
            request_id: word(3),
            value: word(4),
            flags: word(5),
            title: get_str(&bytes[24..24 + TITLE_LEN])?,
            text: get_str(&bytes[24 + TITLE_LEN..])?,
        })
    }

    pub fn hello() -> Message {
        // Host API 2 reads the supported range from `value` (minimum) and
        // `flags` (maximum).
        Message { value: PROTOCOL_VERSION, flags: PROTOCOL_VERSION, ..Message::new(kind::HELLO) }
    }

    pub fn status(text: &str) -> Message {
        Message { text: text.to_owned(), ..Message::new(kind::SET_STATUS) }
    }
}

/// Size and layout of the host's pixel surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    /// Bytes per row, at least `width * 4`.
    pub stride: u32,
    pub slots: u32,
    /// Physical pixels per logical pixel.
    pub scale: f64,
}

impl Geometry {
    /// The browser host's screen, used by the simulator by default.
    pub const PHONE: Geometry = Geometry { width: 1080, height: 2100, stride: 1080 * 4, slots: 2, scale: 3.0 };

    pub fn frame_bytes(&self) -> usize {
        self.stride as usize * self.height as usize
    }

    pub fn total_bytes(&self) -> usize {
        self.frame_bytes() * self.slots as usize
    }

    pub fn valid(&self) -> bool {
        (1..=8192).contains(&self.width)
            && (1..=8192).contains(&self.height)
            && self.stride >= self.width * 4
            && self.stride.is_multiple_of(4)
            && (1..=4).contains(&self.slots)
            && self.scale >= 0.5
            && self.scale <= 8.0
    }

    pub fn to_message(&self) -> Message {
        Message {
            kind: kind::SURFACE,
            request_id: self.stride,
            value: self.width,
            flags: self.height,
            title: "BGRA8888".into(),
            text: format!("slots={} scale={}", self.slots, self.scale),
        }
    }

    pub fn from_message(message: &Message) -> Option<Geometry> {
        if message.kind != kind::SURFACE || message.title != "BGRA8888" {
            return None;
        }
        let mut geometry = Geometry {
            width: message.value,
            height: message.flags,
            stride: message.request_id,
            slots: 2,
            scale: 1.0,
        };
        for pair in message.text.split_whitespace() {
            match pair.split_once('=') {
                Some(("slots", v)) => geometry.slots = v.parse().ok()?,
                Some(("scale", v)) => geometry.scale = v.parse().ok()?,
                _ => {}
            }
        }
        geometry.valid().then_some(geometry)
    }
}

/// The shared pixel surface, mapped read-write.
pub struct Surface {
    base: *mut u8,
    geometry: Geometry,
}

unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    /// Maps `fd`, which must be a regular file (a memfd) of exactly
    /// `geometry.total_bytes()`.
    pub fn map(fd: RawFd, geometry: Geometry) -> io::Result<Surface> {
        let mut info: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut info) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if info.st_mode & libc::S_IFMT != libc::S_IFREG || info.st_size as usize != geometry.total_bytes() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "surface memory does not match its geometry"));
        }
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                geometry.total_bytes(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Surface { base: base.cast(), geometry })
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub fn slot_of(&self, sequence: u32) -> u32 {
        sequence % self.geometry.slots
    }

    /// Slot for `sequence`. Callers must only write a slot the host is not
    /// reading: one whose last frame was answered with `FRAME_DONE`.
    #[allow(clippy::mut_from_ref)]
    pub fn slot(&self, sequence: u32) -> &mut [u8] {
        let bytes = self.geometry.frame_bytes();
        let offset = self.slot_of(sequence) as usize * bytes;
        unsafe { std::slice::from_raw_parts_mut(self.base.add(offset), bytes) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base.cast(), self.geometry.total_bytes()) };
    }
}

/// The control channel.
pub struct Control(UnixDatagram);

impl Control {
    /// Takes ownership of `fd`.
    ///
    /// # Safety
    /// `fd` must be an open socket nothing else owns.
    pub unsafe fn from_fd(fd: RawFd) -> Control {
        Control(UnixDatagram::from(OwnedFd::from_raw_fd(fd)))
    }

    pub fn from_socket(socket: UnixDatagram) -> Control {
        Control(socket)
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    pub fn try_clone(&self) -> io::Result<Control> {
        self.0.try_clone().map(Control)
    }

    pub fn send(&self, message: &Message) -> io::Result<()> {
        let bytes = message.encode();
        loop {
            match self.0.send(&bytes) {
                Ok(n) if n == MESSAGE_LEN => return Ok(()),
                Ok(_) => return Err(io::ErrorKind::WriteZero.into()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Receives one message, or `Ok(None)` when the timeout passes first.
    pub fn recv(&self, timeout: Option<Duration>) -> io::Result<Option<Message>> {
        self.0.set_read_timeout(timeout.map(|t| t.max(Duration::from_micros(1))))?;
        let mut buffer = [0u8; MESSAGE_LEN + 1];
        match self.0.recv(&mut buffer) {
            Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => Message::decode(&buffer[..n])
                .map(Some)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid host message")),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// What the handshake settled.
pub struct Session {
    pub control: Control,
    pub surface: Surface,
    pub features: u32,
    /// Messages that arrived during the handshake (such as the first
    /// `LIFECYCLE`), to be handled once the engine runs.
    pub early: Vec<Message>,
}

/// Runs the assumed handshake: `HELLO`, then `HELLO_ACK` advertising the pixel
/// surface, then `SURFACE`, then maps the surface descriptor.
pub fn connect(control: Control, surface_fd: RawFd, timeout: Duration) -> Result<Session, String> {
    control.send(&Message::hello()).map_err(|e| format!("send HELLO: {e}"))?;
    let deadline = Instant::now() + timeout;
    let mut features = None;
    let mut early = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("AERA did not describe a pixel surface in time".into());
        }
        let Some(message) = control.recv(Some(left)).map_err(|e| format!("handshake: {e}"))? else {
            continue;
        };
        if !kind::from_host(message.kind) {
            return Err(format!("unexpected message {} during handshake", message.kind));
        }
        match (message.kind, features) {
            (kind::HELLO_ACK, None) => {
                if message.flags & FEATURE_PIXEL_SURFACE == 0 {
                    return Err("this AERA build has no pixel surface for plugins".into());
                }
                features = Some(message.flags);
            }
            (kind::SURFACE, Some(features)) => {
                let geometry = Geometry::from_message(&message).ok_or("AERA sent an unusable surface")?;
                let surface = Surface::map(surface_fd, geometry).map_err(|e| format!("surface: {e}"))?;
                return Ok(Session { control, surface, features, early });
            }
            (kind::HELLO_ACK, Some(_)) => return Err("AERA repeated HELLO_ACK".into()),
            (_, None) => return Err("AERA spoke before HELLO_ACK".into()),
            (_, Some(_)) => early.push(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_matches_host_api_2_layout() {
        assert_eq!(MESSAGE_LEN, 1144);
        let message = Message { kind: kind::PRESENT, request_id: 7, value: 1, flags: 0, title: "t".into(), text: "hi".into() };
        let bytes = message.encode();
        assert_eq!(&bytes[..4], b"IP2A"); // A2PI little-endian
        assert_eq!(Message::decode(&bytes).unwrap(), message);
    }

    #[test]
    fn geometry_round_trips() {
        let message = Geometry::PHONE.to_message();
        assert_eq!(Geometry::from_message(&message), Some(Geometry::PHONE));
        let mut bad = message.clone();
        bad.request_id = 16; // stride smaller than a row
        assert_eq!(Geometry::from_message(&bad), None);
    }

    #[test]
    fn kinds_do_not_overlap_host_api_2() {
        assert!(kind::from_worker(kind::PRESENT) && !kind::from_host(kind::PRESENT));
        assert!(kind::from_host(kind::SURFACE) && !kind::from_worker(kind::SURFACE));
        // Host API 2 ends at SET_BACK_ACTION (11) and OPERATION_RESULT (67).
        assert!(kind::PRESENT > 11 && kind::SURFACE > 67);
    }
}
