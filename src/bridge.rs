//! AERA's browser bridge, as defined by `aeraui/features/browser/protocol.hpp`
//! in AERA-Recovery/android_bootable_recovery (branch aera-16.0).
//!
//! fd 3: a sealed memfd with two 1080x2100 ARGB8888 frame slots.
//! fd 4: a `SOCK_SEQPACKET` socket carrying fixed 2072-byte packets.
//! The worker publishes a frame into slot `sequence % 2`, sends `FRAME`, and
//! must not publish again until AERA answers with `ACK` for that sequence.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;

pub const MAGIC: u32 = 0x4152_5032;
pub const WIDTH: i32 = 1080;
pub const HEIGHT: i32 = 2100;
pub const VIEW_WIDTH: i32 = 360;
pub const VIEW_HEIGHT: i32 = 700;
pub const DEVICE_SCALE: f64 = 3.0;
pub const FRAME_BYTES: usize = (WIDTH * HEIGHT * 4) as usize;
pub const FRAME_SLOTS: usize = 2;
pub const SHARED_BYTES: usize = FRAME_BYTES * FRAME_SLOTS;
pub const TEXT_LEN: usize = 2048;
pub const PACKET_LEN: usize = 6 * 4 + TEXT_LEN;

pub const FRAME_FD: RawFd = 3;
pub const CONTROL_FD: RawFd = 4;

/// Packet kinds. Below 32 AERA to worker, 32 and up worker to AERA.
pub mod kind {
    pub const OPEN: u32 = 1;
    pub const BACK: u32 = 2;
    pub const FORWARD: u32 = 3;
    pub const RELOAD: u32 = 4;
    pub const STOP: u32 = 5;
    pub const TOUCH_DOWN: u32 = 6;
    pub const TOUCH_MOVE: u32 = 7;
    pub const TOUCH_UP: u32 = 8;
    pub const KEY: u32 = 9;
    pub const ACK: u32 = 10;
    pub const CLOSE: u32 = 11;
    pub const SET_ZOOM: u32 = 12;
    pub const DOWNLOAD_CANCEL: u32 = 13;
    pub const SET_COOKIE_POLICY: u32 = 14;
    pub const CLEAR_BROWSING_DATA: u32 = 15;
    pub const FRAME: u32 = 32;
    pub const STATUS: u32 = 33;
    pub const ERROR: u32 = 34;
    pub const KEYBOARD_SHOW: u32 = 35;
    pub const KEYBOARD_HIDE: u32 = 36;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Packet {
    pub kind: u32,
    pub sequence: u32,
    pub x: i32,
    pub y: i32,
    pub value: u32,
    pub text: String,
}

impl Packet {
    pub fn new(kind: u32) -> Packet {
        Packet { kind, ..Default::default() }
    }

    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut out = [0u8; PACKET_LEN];
        let words = [MAGIC, self.kind, self.sequence, self.x as u32, self.y as u32, self.value];
        for (i, word) in words.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let mut end = self.text.len().min(TEXT_LEN - 1);
        while !self.text.is_char_boundary(end) {
            end -= 1;
        }
        out[24..24 + end].copy_from_slice(&self.text.as_bytes()[..end]);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Packet> {
        if bytes.len() != PACKET_LEN {
            return None;
        }
        let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        if word(0) != MAGIC {
            return None;
        }
        let text = &bytes[24..];
        let end = text.iter().position(|&b| b == 0)?;
        Some(Packet {
            kind: word(1),
            sequence: word(2),
            x: word(3) as i32,
            y: word(4) as i32,
            value: word(5),
            text: String::from_utf8_lossy(&text[..end]).into_owned(),
        })
    }

    /// The same checks `web::Valid(m, false)` applies on the host side, so
    /// the simulator rejects what AERA would reject.
    pub fn valid_from_host(&self) -> bool {
        use kind::*;
        match self.kind {
            TOUCH_DOWN | TOUCH_MOVE | TOUCH_UP => {
                self.x >= 0 && self.y >= 0 && self.x < VIEW_WIDTH && self.y < VIEW_HEIGHT && self.value < 2
            }
            SET_ZOOM => (50..=300).contains(&self.value),
            DOWNLOAD_CANCEL => self.sequence > 0,
            SET_COOKIE_POLICY => self.value <= 2,
            OPEN | BACK | FORWARD | RELOAD | STOP | KEY | ACK | CLOSE | CLEAR_BROWSING_DATA => true,
            _ => false,
        }
    }

    pub fn valid_from_worker(&self) -> bool {
        use kind::*;
        match self.kind {
            FRAME => self.value as usize == FRAME_BYTES && self.x == WIDTH && self.y == HEIGHT,
            STATUS => self.value <= 100 && (0..=1).contains(&self.x) && (0..=1).contains(&self.y),
            ERROR | KEYBOARD_HIDE => true,
            KEYBOARD_SHOW => self.value <= 10,
            _ => false,
        }
    }
}

/// The shared frame memory, mapped read-write.
pub struct Frames {
    base: *mut u8,
}

unsafe impl Send for Frames {}
unsafe impl Sync for Frames {}

impl Frames {
    /// Maps `fd`, which must be a regular file of exactly [`SHARED_BYTES`].
    pub fn map(fd: RawFd) -> io::Result<Frames> {
        let mut info: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut info) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if info.st_mode & libc::S_IFMT != libc::S_IFREG || info.st_size as usize != SHARED_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame memory has the wrong size"));
        }
        let base = unsafe {
            libc::mmap(std::ptr::null_mut(), SHARED_BYTES, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Frames { base: base.cast() })
    }

    /// Slot for `sequence`. Callers must only write a slot AERA is not
    /// reading: the one after the last acknowledged sequence.
    #[allow(clippy::mut_from_ref)]
    pub fn slot(&self, sequence: u32) -> &mut [u8] {
        let offset = (sequence as usize % FRAME_SLOTS) * FRAME_BYTES;
        unsafe { std::slice::from_raw_parts_mut(self.base.add(offset), FRAME_BYTES) }
    }
}

/// The control socket.
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

    pub fn send(&self, packet: &Packet) -> io::Result<()> {
        let bytes = packet.encode();
        loop {
            match self.0.send(&bytes) {
                Ok(n) if n == PACKET_LEN => return Ok(()),
                Ok(_) => return Err(io::ErrorKind::WriteZero.into()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Receives one packet, or `Ok(None)` when the timeout passes first.
    pub fn recv(&self, timeout: Option<std::time::Duration>) -> io::Result<Option<Packet>> {
        self.0.set_read_timeout(timeout.map(|t| t.max(std::time::Duration::from_micros(1))))?;
        let mut buffer = [0u8; PACKET_LEN + 1];
        match self.0.recv(&mut buffer) {
            Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => Packet::decode(&buffer[..n])
                .map(Some)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid bridge packet")),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_matches_c_struct() {
        assert_eq!(PACKET_LEN, 2072);
        let packet = Packet { kind: kind::FRAME, sequence: 7, x: WIDTH, y: HEIGHT, value: FRAME_BYTES as u32, text: "hi".into() };
        let decoded = Packet::decode(&packet.encode()).unwrap();
        assert_eq!(decoded, packet);
        assert!(decoded.valid_from_worker());
    }

    #[test]
    fn host_validation_matches_recovery() {
        let mut touch = Packet::new(kind::TOUCH_DOWN);
        touch.x = 359;
        touch.y = 699;
        assert!(touch.valid_from_host());
        touch.x = 360;
        assert!(!touch.valid_from_host());
    }
}
