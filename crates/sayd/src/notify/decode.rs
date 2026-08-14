//! Turning a bus message into a `Notification`.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

/// The three fields of a notification `sayd` has any use for.
///
/// The other five in the `Notify` signature are for a notification *daemon* --
/// an icon to draw, a timeout to honour, actions to offer, an id to replace.
/// `sayd` draws nothing and answers nothing, so it keeps what can be spoken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub app_name: String,
    pub summary: String,
    pub body: String,
}

/// The full `Notify` signature, `(susssasa{sv}i)`.
///
/// Named as one type because it must be deserialized whole: the body is a
/// single struct, and asking for a prefix of it fails with a signature
/// mismatch rather than returning the fields that were wanted.
type NotifyArgs = (
    String,
    u32,
    String,
    String,
    String,
    Vec<String>,
    HashMap<String, OwnedValue>,
    i32,
);

/// Decode a `Notify` method call, or `None` if this message is not one.
///
/// `None` rather than a `Result` because every non-notification outcome is
/// handled identically by the caller -- skip it. That includes the daemon's
/// own bus traffic, which a monitor connection receives alongside what its
/// match rule asked for.
pub fn decode(msg: &zbus::Message) -> Option<Notification> {
    let header = msg.header();
    if header.member().map(|m| m.as_str()) != Some("Notify") {
        return None;
    }
    if header.interface().map(|i| i.as_str()) != Some("org.freedesktop.Notifications") {
        return None;
    }
    let body = msg.body();
    let (app_name, _replaces_id, _icon, summary, text, _actions, _hints, _timeout): NotifyArgs =
        body.deserialize().ok()?;
    Some(Notification {
        app_name,
        summary,
        body: text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::Value;

    fn notify_message(app: &str, summary: &str, body: &str) -> zbus::Message {
        zbus::Message::method_call("/org/freedesktop/Notifications", "Notify")
            .expect("builder")
            .interface("org.freedesktop.Notifications")
            .expect("interface")
            .build(&(
                app,
                0u32,
                "",
                summary,
                body,
                Vec::<String>::new(),
                HashMap::<String, Value>::new(),
                5000i32,
            ))
            .expect("message")
    }

    #[test]
    fn a_notify_call_decodes_to_its_three_useful_fields() {
        let m = notify_message("Signal", "Alice sent a message", "see you at five");
        let n = decode(&m).expect("decodes");
        assert_eq!(n.app_name, "Signal");
        assert_eq!(n.summary, "Alice sent a message");
        assert_eq!(n.body, "see you at five");
    }

    /// A monitor connection receives its own bus traffic -- the first message
    /// off the stream in the spike was `NameLost`, not a notification. The
    /// match rule is not the only filter.
    #[test]
    fn a_message_that_is_not_a_notify_call_is_ignored() {
        let m = zbus::Message::method_call("/org/freedesktop/DBus", "NameLost")
            .expect("builder")
            .interface("org.freedesktop.DBus")
            .expect("interface")
            .build(&("sh.sayd.Sayd",))
            .expect("message");
        assert!(decode(&m).is_none());
    }

    /// One malformed sender must not stop narration, so a body that does not
    /// match the signature is skipped rather than propagated as an error.
    #[test]
    fn a_notify_call_with_the_wrong_body_is_skipped() {
        let m = zbus::Message::method_call("/org/freedesktop/Notifications", "Notify")
            .expect("builder")
            .interface("org.freedesktop.Notifications")
            .expect("interface")
            .build(&("only one field",))
            .expect("message");
        assert!(decode(&m).is_none());
    }
}
