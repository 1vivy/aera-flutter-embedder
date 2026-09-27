//! The `flutter/textinput` channel, driven by AERA's native keyboard.
//!
//! AERA shows its keyboard when the worker sends `KEYBOARD_SHOW` and delivers
//! each key as a `KEY` packet holding one Unicode code point (8 is backspace,
//! 13 is enter). Flutter expects an input method that keeps the editing state
//! and reports it back, so this module is that input method.

use serde_json::{json, Value};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditingState {
    pub text: String,
    /// Selection in UTF-16 code units, as Flutter reports it.
    pub base: i64,
    pub extent: i64,
}

impl EditingState {
    fn from_json(value: &Value) -> EditingState {
        EditingState {
            text: value["text"].as_str().unwrap_or_default().to_owned(),
            base: value["selectionBase"].as_i64().unwrap_or(-1),
            extent: value["selectionExtent"].as_i64().unwrap_or(-1),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "text": self.text,
            "selectionBase": self.base,
            "selectionExtent": self.extent,
            "selectionAffinity": "TextAffinity.downstream",
            "selectionIsDirectional": false,
            "composingBase": -1,
            "composingExtent": -1,
        })
    }

    /// Byte range of the selection, clamped to the text.
    fn selection(&self) -> (usize, usize) {
        let units = self.text.encode_utf16().count() as i64;
        let clamp = |v: i64| if v < 0 { units } else { v.min(units) };
        let (a, b) = (clamp(self.base), clamp(self.extent));
        (self.byte_at(a.min(b) as usize), self.byte_at(a.max(b) as usize))
    }

    fn byte_at(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (index, ch) in self.text.char_indices() {
            if units >= utf16 {
                return index;
            }
            units += ch.len_utf16();
        }
        self.text.len()
    }

    fn set_cursor_byte(&mut self, byte: usize) {
        let units = self.text[..byte].encode_utf16().count() as i64;
        self.base = units;
        self.extent = units;
    }

    pub fn insert(&mut self, text: &str) {
        let (start, end) = self.selection();
        self.text.replace_range(start..end, text);
        self.set_cursor_byte(start + text.len());
    }

    pub fn backspace(&mut self) {
        let (start, end) = self.selection();
        if start != end {
            self.text.replace_range(start..end, "");
            self.set_cursor_byte(start);
        } else if let Some((index, _)) = self.text[..start].char_indices().next_back() {
            self.text.replace_range(index..start, "");
            self.set_cursor_byte(index);
        }
    }
}

/// What the embedder must do after a text input call or key.
#[derive(Debug, PartialEq)]
pub enum Effect {
    ShowKeyboard { purpose: u32 },
    HideKeyboard,
    /// Send this method call to Flutter on `flutter/textinput`.
    ToFlutter(Value),
}

#[derive(Debug, Default)]
pub struct TextInput {
    client: Option<i64>,
    multiline: bool,
    action: String,
    purpose: u32,
    state: EditingState,
}

impl TextInput {
    /// Handles a method call from Flutter and returns the effects to apply.
    pub fn handle(&mut self, method: &str, args: &Value) -> Vec<Effect> {
        match method {
            "TextInput.setClient" => {
                self.client = args[0].as_i64();
                let config = &args[1];
                let kind = config["inputType"]["name"].as_str().unwrap_or("");
                self.multiline = kind == "TextInputType.multiline";
                // Purposes follow WPE's input purpose enum, which is what
                // AERA's keyboard switches on (2 digits, 9 number).
                self.purpose = match kind {
                    "TextInputType.number" | "TextInputType.phone" => 2,
                    _ => 0,
                };
                self.action = config["inputAction"].as_str().unwrap_or("TextInputAction.done").to_owned();
                Vec::new()
            }
            "TextInput.clearClient" => {
                self.client = None;
                vec![Effect::HideKeyboard]
            }
            "TextInput.setEditingState" => {
                self.state = EditingState::from_json(args);
                Vec::new()
            }
            "TextInput.show" if self.client.is_some() => vec![Effect::ShowKeyboard { purpose: self.purpose }],
            "TextInput.hide" => vec![Effect::HideKeyboard],
            _ => Vec::new(),
        }
    }

    /// Applies one key from AERA's keyboard.
    pub fn key(&mut self, code_point: u32) -> Vec<Effect> {
        let Some(client) = self.client else { return Vec::new() };
        match code_point {
            8 => self.state.backspace(),
            13 if !self.multiline => {
                return vec![Effect::ToFlutter(json!({
                    "method": "TextInputClient.performAction",
                    "args": [client, self.action],
                }))];
            }
            13 => self.state.insert("\n"),
            other => match char::from_u32(other) {
                Some(ch) if !ch.is_control() => self.state.insert(ch.encode_utf8(&mut [0; 4])),
                _ => return Vec::new(),
            },
        }
        vec![Effect::ToFlutter(json!({
            "method": "TextInputClient.updateEditingState",
            "args": [client, self.state.to_json()],
        }))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attached(kind: &str) -> TextInput {
        let mut input = TextInput::default();
        input.handle("TextInput.setClient", &json!([3, {"inputType": {"name": kind}, "inputAction": "TextInputAction.go"}]));
        input.handle("TextInput.setEditingState", &json!({"text": "héllo", "selectionBase": 5, "selectionExtent": 5}));
        input
    }

    #[test]
    fn typing_and_backspace_follow_the_cursor() {
        let mut input = attached("TextInputType.text");
        input.key('!' as u32);
        assert_eq!(input.state.text, "héllo!");
        input.key(8);
        input.key(8);
        assert_eq!(input.state.text, "héll");
        assert_eq!((input.state.base, input.state.extent), (4, 4));
    }

    #[test]
    fn selection_is_replaced() {
        let mut input = attached("TextInputType.text");
        input.handle("TextInput.setEditingState", &json!({"text": "abcdef", "selectionBase": 1, "selectionExtent": 4}));
        input.key('X' as u32);
        assert_eq!(input.state.text, "aXef");
    }

    #[test]
    fn enter_performs_action_unless_multiline() {
        let mut input = attached("TextInputType.text");
        let effects = input.key(13);
        assert!(matches!(&effects[0], Effect::ToFlutter(v) if v["method"] == "TextInputClient.performAction" && v["args"][1] == "TextInputAction.go"));
        let mut input = attached("TextInputType.multiline");
        input.key(13);
        assert_eq!(input.state.text, "héllo\n");
    }

    #[test]
    fn show_uses_numeric_keyboard_for_numbers() {
        let mut input = attached("TextInputType.number");
        assert_eq!(input.handle("TextInput.show", &Value::Null), vec![Effect::ShowKeyboard { purpose: 2 }]);
    }

    #[test]
    fn keys_without_client_are_ignored() {
        assert!(TextInput::default().key('a' as u32).is_empty());
    }
}
