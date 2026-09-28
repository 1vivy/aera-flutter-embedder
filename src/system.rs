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
//! Calls from Dart use the standard JSON method codec.

use crate::host::{kind, Message};
use serde_json::{json, Value};

pub const CHANNEL: &str = "aera/system";

/// AERA's portrait keyboard is 760 px of a 2708 px high screen in the
/// browser (`web_scene.cpp`); used only until the host reports its own.
const KEYBOARD_SHARE: f64 = 760.0 / 2708.0;

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
                json!([{"keyboardVisible": self.inset > 0.0, "keyboardInset": self.inset}]),
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
    fn navigation_state_is_accepted() {
        let mut system = System::new(2100);
        let (reply, effects) = system.call("setNavigationState", &json!({"canGoBack": true}));
        assert_eq!(reply, json!([null]));
        assert!(effects.is_empty());
    }
}
