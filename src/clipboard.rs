//! Flutter's `Clipboard`, on `flutter/platform`.
//!
//! AERA has no clipboard an app can reach, so text copied in the app is kept
//! in a file in the app's `/tmp`: it survives while AERA keeps the app in
//! memory and is shared with nothing else.

use serde_json::{json, Value};

fn file() -> std::path::PathBuf {
    std::env::temp_dir().join("aera-flutter-clipboard")
}

/// Answers a `Clipboard.*` method, or `None` for any other.
pub fn handle(method: &str, args: &Value) -> Option<Value> {
    match method {
        "Clipboard.setData" => {
            let _ = std::fs::write(file(), args["text"].as_str().unwrap_or_default());
            Some(json!([null]))
        }
        "Clipboard.getData" => Some(match std::fs::read_to_string(file()) {
            Ok(text) => json!([{"text": text}]),
            Err(_) => json!([null]),
        }),
        "Clipboard.hasStrings" => {
            let has = std::fs::metadata(file()).is_ok_and(|m| m.len() > 0);
            Some(json!([{"value": has}]))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_and_pastes() {
        let _ = std::fs::remove_file(file());
        assert_eq!(handle("Clipboard.hasStrings", &Value::Null), Some(json!([{"value": false}])));
        handle("Clipboard.setData", &json!({"text": "héllo"}));
        assert_eq!(handle("Clipboard.getData", &json!("text/plain")), Some(json!([{"text": "héllo"}])));
        assert!(handle("SystemSound.play", &Value::Null).is_none());
        let _ = std::fs::remove_file(file());
    }
}
