//! The `aera/system` channel: AERA's own chrome and gestures, for Flutter.
//!
//! AERA draws a top bar (Back, Forward, Reload, Home, an address field) and
//! owns the edge-back gesture and the keyboard. This module is the glue
//! between those and a Flutter app, the way Flutter's Android shell glues
//! the system back button and the soft keyboard:
//!
//! - AERA routes the back gesture and enables its Back button only while the
//!   worker's last `STATUS` said it can go back (`web_scene.cpp`, `CanBack`).
//!   The app reports that with `setNavigationState`; otherwise Back leaves the app.
//! - Forward, Reload, Stop, Home and the address field arrive as packets and
//!   go to Dart as `onForward`, `onReload`, `onStop` and `onOpen`.
//! - A pinch on the viewport arrives as `SET_ZOOM`, sent on as `onZoom`.
//! - While the keyboard shows, the app gets a bottom view inset, as on
//!   Android. AERA does not report closing the keyboard, but it stops
//!   forwarding touches while the keyboard is up, so the next touch means
//!   it is gone.
//!
//! Calls from Dart use the standard JSON method codec.

use crate::bridge::{kind, Packet, HEIGHT, VIEW_HEIGHT};
use serde_json::{json, Value};

pub const CHANNEL: &str = "aera/system";

/// AERA's portrait keyboard is 760 px tall and sits at the bottom of the
/// 2708 px high viewport that shows our 2100 px frame
/// (`web_scene.cpp`: `kBrowserViewportHeight`, `lv_obj_set_size(keyboard)`).
pub const KEYBOARD_INSET: f64 = (760 * HEIGHT) as f64 / 2708.0;

#[derive(Debug, Default)]
pub struct System {
    can_back: bool,
    can_forward: bool,
    progress: u32,
    address: String,
    keyboard: bool,
}

/// What the embedder must do after an event.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Send to AERA.
    Host(Packet),
    /// Invoke a method on `aera/system` in Dart.
    Dart(Value),
    /// The bottom inset changed; resend window metrics.
    Metrics,
}

impl System {
    pub fn bottom_inset(&self) -> f64 {
        if self.keyboard { KEYBOARD_INSET } else { 0.0 }
    }

    fn status(&self) -> Effect {
        let mut packet = Packet::new(kind::STATUS);
        packet.x = self.can_back as i32;
        packet.y = self.can_forward as i32;
        packet.value = self.progress;
        packet.text = self.address.clone();
        Effect::Host(packet)
    }

    /// A method call from Dart. Returns the reply and the effects.
    pub fn call(&mut self, method: &str, args: &Value) -> (Value, Vec<Effect>) {
        match method {
            "setNavigationState" => {
                if let Some(back) = args["canGoBack"].as_bool() {
                    self.can_back = back;
                }
                if let Some(forward) = args["canGoForward"].as_bool() {
                    self.can_forward = forward;
                }
                (json!([null]), vec![self.status()])
            }
            "setStatus" => {
                if let Some(progress) = args["progress"].as_u64() {
                    self.progress = progress.min(100) as u32;
                }
                if let Some(address) = args["address"].as_str() {
                    self.address = address.to_owned();
                }
                (json!([null]), vec![self.status()])
            }
            "getState" => (
                json!([{"keyboardVisible": self.keyboard, "keyboardInset": self.bottom_inset()}]),
                Vec::new(),
            ),
            _ => (Value::Null, Vec::new()),
        }
    }

    /// A packet from AERA. Returns the effects; packets this module does not
    /// own return nothing.
    pub fn from_host(&mut self, packet: &Packet) -> Vec<Effect> {
        let dart = |method: &str, args: Value| Effect::Dart(json!({"method": method, "args": args}));
        match packet.kind {
            kind::FORWARD => vec![dart("onForward", Value::Null)],
            kind::RELOAD => vec![dart("onReload", Value::Null)],
            kind::STOP => vec![dart("onStop", Value::Null)],
            kind::OPEN => vec![dart("onOpen", json!(packet.text))],
            kind::SET_ZOOM => vec![dart("onZoom", json!(packet.value))],
            kind::TOUCH_DOWN if self.keyboard && packet.y < VIEW_HEIGHT => self.keyboard_changed(false),
            _ => Vec::new(),
        }
    }

    /// The embedder asked AERA to show or hide its keyboard, or learned
    /// that it is gone.
    pub fn keyboard_changed(&mut self, visible: bool) -> Vec<Effect> {
        if self.keyboard == visible {
            return Vec::new();
        }
        self.keyboard = visible;
        vec![
            Effect::Metrics,
            Effect::Dart(json!({"method": "onKeyboard", "args": {"visible": visible, "inset": self.bottom_inset()}})),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_state_becomes_status() {
        let mut system = System::default();
        let (_, effects) = system.call("setNavigationState", &json!({"canGoBack": true}));
        let Effect::Host(packet) = &effects[0] else { panic!() };
        assert_eq!((packet.kind, packet.x, packet.y), (kind::STATUS, 1, 0));
        assert!(packet.valid_from_worker());
        let (_, effects) = system.call("setStatus", &json!({"progress": 250, "address": "aera://app"}));
        let Effect::Host(packet) = &effects[0] else { panic!() };
        assert_eq!((packet.x, packet.value, packet.text.as_str()), (1, 100, "aera://app"));
    }

    #[test]
    fn touch_after_keyboard_means_it_closed() {
        let mut system = System::default();
        assert_eq!(system.keyboard_changed(true).len(), 2);
        assert!((system.bottom_inset() - 589.3).abs() < 0.1);
        let effects = system.from_host(&Packet::new(kind::TOUCH_DOWN));
        assert_eq!(effects[0], Effect::Metrics);
        assert_eq!(system.bottom_inset(), 0.0);
        assert!(system.from_host(&Packet::new(kind::TOUCH_DOWN)).is_empty());
    }

    #[test]
    fn chrome_buttons_reach_dart() {
        let mut system = System::default();
        let mut open = Packet::new(kind::OPEN);
        open.text = "https://example.org".into();
        assert_eq!(
            system.from_host(&open),
            vec![Effect::Dart(json!({"method": "onOpen", "args": "https://example.org"}))]
        );
        assert!(system.from_host(&Packet::new(kind::ACK)).is_empty());
    }
}
