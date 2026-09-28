//! The `aera/system` channel: AERA's gestures and keyboard, for Flutter.
//!
//! The same channel as on the browser-slot branch, so one Dart app runs on
//! both, but the generic pixel host draws no chrome around the app:
//!
//! - AERA's edge-back gesture arrives as `BACK` and pops the navigator;
//!   popping the last route closes the app. `setNavigationState` and
//!   `setStatus` are accepted and remembered, but there is no Back button,
//!   address field or progress bar to update.
//! - While the keyboard shows, the app gets a bottom view inset, as on
//!   Android. ASSUMED: the host reports the keyboard's height with
//!   `KEYBOARD_INSET` whenever it shows, hides or resizes (asked for in
//!   aera-flutter-demo#1). Until the host has sent one, the embedder uses
//!   the browser keyboard's proportions when it asks for the keyboard.
//!
//! - The surface covers the whole screen, rounded corners, camera hole and
//!   the Recents swipe-up strip included. `getState` returns a safe area
//!   (`padding`) and AERA's gesture areas (`gestureInsets`), which
//!   `AeraScope` puts in `MediaQuery`, as on the browser-slot branch.
//!   These are fixed, good-enough values for a typical phone, not asked of
//!   the host: AERA's status bar height at the top and 96 screen px at the
//!   bottom, scaled from the 3168 px screen.
//!
//! Calls from Dart use the standard JSON method codec.

use crate::host::{kind, Message};
use serde_json::{json, Value};

pub const CHANNEL: &str = "aera/system";

/// AERA's portrait keyboard is 760 px of a 2708 px high screen in the
/// browser (`web_scene.cpp`); used only until the host reports its own.
const KEYBOARD_SHARE: f64 = 760.0 / 2708.0;

/// AERA's screen height, which the constants below are measured against.
const SCREEN_HEIGHT: f64 = 3168.0;
/// Top safe area: AERA's own status bar height (165 px,
/// `aera_ui_host.cpp`), which clears the camera hole and the top corners.
const TOP_PADDING: f64 = 165.0;
/// Bottom safe area: the Recents strip, `max(64, 3168 / 44)` = 72 px
/// (`engine.cpp`, `bottom_edge`), plus room for a typical phone's corner
/// radius (about 100 to 140 px on a 1440 px wide screen).
const BOTTOM_PADDING: f64 = 96.0;
/// AERA's gestures [left, top, right, bottom] in screen px: the back edges
/// (`max(72, 1440 / 20)`) and the Recents strip.
const GESTURE_INSETS: [f64; 4] = [72.0, 0.0, 72.0, 72.0];

#[derive(Debug)]
pub struct System {
    surface_height: f64,
    can_back: bool,
    can_forward: bool,
    /// Bottom inset in surface pixels; 0 while the keyboard is hidden.
    inset: f64,
    /// The host has reported the keyboard itself, so its word is final.
    host_reports: bool,
}

/// What the embedder must do after an event.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Send to AERA.
    Host(Message),
    /// Invoke a method on `aera/system` in Dart.
    Dart(Value),
    /// The bottom inset changed; resend window metrics.
    Metrics,
}

impl System {
    pub fn new(surface_height: u32) -> System {
        System { surface_height: surface_height as f64, can_back: false, can_forward: false, inset: 0.0, host_reports: false }
    }

    pub fn bottom_inset(&self) -> f64 {
        self.inset
    }

    /// Screen pixels to surface pixels.
    fn screen(&self, insets: [f64; 4]) -> [f64; 4] {
        insets.map(|v| v * self.surface_height / SCREEN_HEIGHT)
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
                (json!([null]), Vec::new())
            }
            "setStatus" => (json!([null]), Vec::new()),
            "getState" => (
                json!([{
                    "keyboardVisible": self.inset > 0.0,
                    "keyboardInset": self.inset,
                    "padding": self.screen([0.0, TOP_PADDING, 0.0, BOTTOM_PADDING]),
                    "gestureInsets": self.screen(GESTURE_INSETS),
                }]),
                Vec::new(),
            ),
            _ => (Value::Null, Vec::new()),
        }
    }

    /// A message from AERA. Messages this module does not own return nothing.
    pub fn from_host(&mut self, message: &Message) -> Vec<Effect> {
        match message.kind {
            kind::KEYBOARD_INSET => {
                self.host_reports = true;
                self.set_inset((message.value as f64).min(self.surface_height))
            }
            _ => Vec::new(),
        }
    }

    /// The embedder asked AERA to show or hide its keyboard.
    pub fn keyboard_changed(&mut self, visible: bool) -> Vec<Effect> {
        if self.host_reports {
            return Vec::new();
        }
        self.set_inset(if visible { (self.surface_height * KEYBOARD_SHARE).round() } else { 0.0 })
    }

    fn set_inset(&mut self, inset: f64) -> Vec<Effect> {
        if self.inset == inset {
            return Vec::new();
        }
        self.inset = inset;
        let visible = inset > 0.0;
        vec![
            Effect::Metrics,
            Effect::Dart(json!({"method": "onKeyboard", "args": {"visible": visible, "inset": inset}})),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_until_the_host_reports() {
        let mut system = System::new(2100);
        assert_eq!(system.keyboard_changed(true).len(), 2);
        assert!((system.bottom_inset() - 589.0).abs() < 0.1);
        let report = Message { value: 700, ..Message::new(kind::KEYBOARD_INSET) };
        assert_eq!(system.from_host(&report)[0], Effect::Metrics);
        assert_eq!(system.bottom_inset(), 700.0);
        // From now on only the host moves the inset.
        assert!(system.keyboard_changed(false).is_empty());
        assert_eq!(system.from_host(&Message::new(kind::KEYBOARD_INSET)).len(), 2);
        assert_eq!(system.bottom_inset(), 0.0);
    }

    #[test]
    fn state_carries_the_safe_area() {
        let (reply, _) = System::new(3168).call("getState", &Value::Null);
        assert_eq!(reply[0]["padding"], json!([0.0, 165.0, 0.0, 96.0]));
        assert_eq!(reply[0]["gestureInsets"][0], json!(72.0));
    }

    #[test]
    fn navigation_state_is_accepted() {
        let mut system = System::new(2100);
        let (reply, effects) = system.call("setNavigationState", &json!({"canGoBack": true}));
        assert_eq!(reply, json!([null]));
        assert!(effects.is_empty());
    }
}
