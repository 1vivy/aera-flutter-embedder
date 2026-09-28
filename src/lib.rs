//! Flutter embedder for AERA Recovery's browser slot.
//!
//! The binary `aera-browser-worker` is the embedder; `aera-host-sim` plays
//! AERA's side of the bridge on a PC so the embedder can be tested without a
//! phone.

pub mod bridge;
mod clipboard;
pub mod engine;
#[allow(dead_code)]
mod ffi;
pub mod gl;
pub mod system;
pub mod text_input;
pub mod vk;
