//! Flutter embedder for AERA Recovery's generic pixel + GPU plugin host.
//!
//! `aera-plugin` is the executable AERA starts; it enters the runtime's own
//! glibc loader and runs `aera-flutter`, the embedder. `aera-host-sim` plays
//! AERA's side on a PC so the embedder can be tested without a phone.
//!
//! The host is not released yet: [`host`] holds every assumption about it.

pub mod engine;
#[allow(dead_code)]
mod ffi;
pub mod gl;
pub mod host;
pub mod recovery;
pub mod text_input;
pub mod vk;
