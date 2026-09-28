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
//! - AERA's viewport runs to the bottom of the screen, where the display's
//!   rounded corners and the Recents swipe-up strip sit. The app gets that
//!   strip as a bottom safe area (`getState` → `padding`) and the back edges
//!   and the strip as gesture insets; `AeraScope` puts both in `MediaQuery`,
//!   so `SafeArea`, `Scaffold` and `NavigationBar` keep clear, as on Android.
//!   The embedder API has no padding field, hence the round trip through Dart.
//!
//! Calls from Dart use the standard JSON method codec.

use crate::bridge::{kind, Packet, HEIGHT, VIEW_HEIGHT};
use serde_json::{json, Value};

pub const CHANNEL: &str = "aera/system";

/// AERA's portrait keyboard is 760 px tall and sits at the bottom of the
/// 2708 px high viewport that shows our 2100 px frame
/// (`web_scene.cpp`: `kBrowserViewportHeight`, `lv_obj_set_size(keyboard)`).
pub const KEYBOARD_INSET: f64 = (760 * HEIGHT) as f64 / 2708.0;

/// Frame pixels per screen pixel: the 2708 px high viewport shows 2100.
const SCALE: f64 = HEIGHT as f64 / 2708.0;

/// The bottom safe area, in frame pixels. AERA takes touches in the bottom
/// `max(64, 3168 / 44)` = 72 screen px for its Recents swipe
/// (`engine.cpp`, `bottom_edge`), and the display's rounded corners clip
/// the bottom rows. 96 screen px clears both, for a typical phone's corner
/// radius (about 100 to 140 px on a 1440 px wide screen). It is about 25
/// logical px, near Android's 24 dp gesture bar, and a fixed value: AERA is
/// not asked for it.
pub const BOTTOM_PADDING: f64 = 96.0 * SCALE;

/// AERA's own gestures, in frame pixels [left, top, right, bottom]: the back
/// edges are `max(72, 1440 / 20)` = 72 screen px from each side, and the
/// viewport starts 24 px in; the bottom is the Recents strip.
pub const GESTURE_INSETS: [f64; 4] = [48.0 * SCALE, 0.0, 48.0 * SCALE, 72.0 * SCALE];

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
                json!([{
                    "keyboardVisible": self.keyboard,
                    "keyboardInset": self.bottom_inset(),
                    "padding": [0.0, 0.0, 0.0, BOTTOM_PADDING],
                    "gestureInsets": GESTURE_INSETS,
                }]),
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
    fn state_carries_the_safe_area() {
        let (reply, _) = System::default().call("getState", &Value::Null);
        let bottom = reply[0]["padding"][3].as_f64().unwrap();
        assert!((bottom - 74.4).abs() < 0.1, "{bottom}");
        assert!((reply[0]["gestureInsets"][0].as_f64().unwrap() - 37.2).abs() < 0.1);
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
