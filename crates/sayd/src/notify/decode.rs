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

/// The outcome of trying to decode one message off the monitor stream.
///
/// A plain `Option<Notification>` used to stand in for this, but it could not
/// tell two very different outcomes apart: "this was never a `Notify` call"
/// (the daemon's own bus traffic, `NameLost` and the like -- the ordinary,
/// expected majority of what a monitor connection sees, and not worth a
/// word) versus "this *was* a `Notify` call and its body did not deserialize"
/// (spec §2: "skip that message and count it" -- a caller's malformed body
/// must never stop narration, but it must not vanish uncounted either). The
/// caller cannot draw that line itself without re-checking member and
/// interface a second time, so `decode` draws it once, here.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    /// Not a `Notify` method call at all.
    Skip,
    /// A `Notify` call whose body did not match the eight-field signature.
    Malformed,
    /// Decoded successfully.
    Notification(Notification),
}

/// Decode one bus message as a `Notify` method call.
///
/// The message type is checked first, not only member and interface: a
/// *signal* on `org.freedesktop.Notifications` with member `Notify` is not a
/// call this daemon should ever try to answer as one (freedesktop's own
/// `NotificationClosed`/`ActionInvoked` share the interface but not the
/// member, so this is defence in depth rather than a case the spike actually
/// produced -- measured on `dbus-daemon`, a unicast signal with a matching
/// member is not even routed to a monitor; dbus-broker was not measured).
pub fn decode(msg: &zbus::Message) -> Decoded {
    let header = msg.header();
    if header.message_type() != zbus::message::Type::MethodCall {
        return Decoded::Skip;
    }
    if header.member().map(|m| m.as_str()) != Some("Notify") {
        return Decoded::Skip;
    }
    if header.interface().map(|i| i.as_str()) != Some("org.freedesktop.Notifications") {
        return Decoded::Skip;
    }
    let body = msg.body();
    let Ok((app_name, _replaces_id, _icon, summary, text, _actions, _hints, _timeout)) =
        body.deserialize::<NotifyArgs>()
    else {
        return Decoded::Malformed;
    };
    Decoded::Notification(Notification {
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
        let Decoded::Notification(n) = decode(&m) else {
            panic!("expected a decoded notification");
        };
        assert_eq!(n.app_name, "Signal");
        assert_eq!(n.summary, "Alice sent a message");
        assert_eq!(n.body, "see you at five");
    }

    /// A monitor connection receives its own bus traffic -- the first message
    /// off the stream in the spike was `NameLost`, not a notification. The
    /// match rule is not the only filter, and this must be `Skip`, not
    /// `Malformed`: it was never a `Notify` call in the first place, so it is
    /// not worth counting as one that failed to decode.
    #[test]
    fn a_message_that_is_not_a_notify_call_is_ignored() {
        let m = zbus::Message::method_call("/org/freedesktop/DBus", "NameLost")
            .expect("builder")
            .interface("org.freedesktop.DBus")
            .expect("interface")
            .build(&("sh.sayd.Sayd",))
            .expect("message");
        assert_eq!(decode(&m), Decoded::Skip);
    }

    /// One malformed sender must not stop narration, so a body that does not
    /// match the signature is skipped rather than propagated as an error --
    /// but as `Malformed`, not `Skip`, since this genuinely was a `Notify`
    /// call and spec §2 asks for it to be counted.
    #[test]
    fn a_notify_call_with_the_wrong_body_is_malformed_not_skipped() {
        let m = zbus::Message::method_call("/org/freedesktop/Notifications", "Notify")
            .expect("builder")
            .interface("org.freedesktop.Notifications")
            .expect("interface")
            .build(&("only one field",))
            .expect("message");
        assert_eq!(decode(&m), Decoded::Malformed);
    }

    /// Defence in depth (Minor 6): a *signal* sharing the interface and
    /// member of a `Notify` call -- not something the spike ever produced,
    /// but cheap to rule out rather than trust the match rule and `decode`'s
    /// other two checks to be the only things standing between this and a
    /// bogus deserialize attempt.
    #[test]
    fn a_signal_with_the_notify_member_is_skipped_not_decoded() {
        let m = zbus::Message::signal(
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
        )
        .expect("builder")
        .build(&("only one field",))
        .expect("message");
        assert_eq!(decode(&m), Decoded::Skip);
    }
}
