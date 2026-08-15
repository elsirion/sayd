//! Turning a bus message into a `Notification`.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

/// The fields of a notification `sayd` has any use for.
///
/// The rest of the `Notify` signature -- a replaces-id, actions to offer, a
/// timeout to honour -- is for a notification *daemon*, and `sayd` is not
/// one: it draws nothing and answers nothing. `summary` and `body` remain
/// the two fields that get spoken; the other three are kept for a reason
/// that has nothing to do with speaking anything -- `notify::seen::record`
/// remembers them against `app_name` so the settings window can suggest an
/// application to allowlist next to the icon it actually notifies with,
/// instead of a generic placeholder.
///
/// Three icon fields rather than one, because `app_icon` alone is almost
/// never the icon. Measured against a real notification server (a private
/// `dbus-daemon` with a stub owning `org.freedesktop.Notifications`):
///
/// - `notify-send -a X "hi"` sends `app_icon = ""`.
/// - `notify-send -a X -i dialog-information "hi"` sends `app_icon = ""`
///   and puts `dialog-information` in the **`image-path`** hint.
/// - A GLib `GNotification` -- which is every GTK4/GNOME application, and
///   what Firefox and the Electron applications end up going through --
///   sends `app_icon = ""`, the application's own app-id in the
///   **`desktop-entry`** hint, and its icon in `image-path`.
///
/// Only `notify-send -n/--app-icon` (rare) and some Qt applications fill
/// `app_icon` at all, so a window drawn from `app_icon` alone shows the
/// fallback glyph for essentially every real sender. `desktop-entry` comes
/// first of the three because it is an app-id that can be handed straight to
/// an icon theme, and it is the one an application is least likely to have
/// pointed at a temporary file it then deletes.
///
/// What is *not* kept is as deliberate: the `hints` map goes no further than
/// this function, because `image-data` is a raw pixel buffer -- a whole
/// decoded image per notification -- and retaining a map to reach two
/// strings in it would retain that too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub app_name: String,
    /// The `desktop-entry` hint: an application id (`org.gnome.Fractal`),
    /// resolvable through the icon theme. Empty when the sender sent none.
    pub desktop_entry: String,
    /// The `image-path` hint: an icon *name* or a path, either shape (see
    /// `settings::model::icon_source`). Empty when the sender sent none.
    pub image_path: String,
    /// The `app_icon` argument, the field the spec nominally puts this in
    /// and the field almost nothing fills. Empty when the sender sent none.
    pub app_icon: String,
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
    let Ok((app_name, _replaces_id, app_icon, summary, text, _actions, hints, _timeout)) =
        body.deserialize::<NotifyArgs>()
    else {
        return Decoded::Malformed;
    };
    Decoded::Notification(Notification {
        app_name,
        desktop_entry: hint_str(&hints, "desktop-entry"),
        image_path: hint_str(&hints, "image-path"),
        app_icon,
        summary,
        body: text,
    })
}

/// One hint, if it is present *and* a string.
///
/// Anything else -- a hint of the wrong type, or one absent entirely -- is
/// the empty string rather than an error: hints are optional by
/// construction, every sender sends a different subset of them, and a
/// notification whose `desktop-entry` arrived as an integer is still a
/// notification worth speaking. The two this asks for (`desktop-entry`,
/// `image-path`) are both `s` per the freedesktop specification.
///
/// The borrowed `&str` is copied out here rather than returned: the value it
/// points into belongs to the `hints` map, which this function is the last
/// place to hold (see [`Notification`]'s doc comment on `image-data`).
fn hint_str(hints: &HashMap<String, OwnedValue>, key: &str) -> String {
    hints
        .get(key)
        .and_then(|v| v.downcast_ref::<&str>().ok())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::Value;

    fn notify_message(app: &str, icon: &str, summary: &str, body: &str) -> zbus::Message {
        notify_message_with_hints(app, icon, summary, body, HashMap::new())
    }

    fn notify_message_with_hints(
        app: &str,
        icon: &str,
        summary: &str,
        body: &str,
        hints: HashMap<String, Value<'_>>,
    ) -> zbus::Message {
        zbus::Message::method_call("/org/freedesktop/Notifications", "Notify")
            .expect("builder")
            .interface("org.freedesktop.Notifications")
            .expect("interface")
            .build(&(
                app,
                0u32,
                icon,
                summary,
                body,
                Vec::<String>::new(),
                hints,
                5000i32,
            ))
            .expect("message")
    }

    #[test]
    fn a_notify_call_decodes_to_its_useful_fields() {
        let m = notify_message(
            "Signal",
            "signal-desktop",
            "Alice sent a message",
            "see you at five",
        );
        let Decoded::Notification(n) = decode(&m) else {
            panic!("expected a decoded notification");
        };
        assert_eq!(n.app_name, "Signal");
        assert_eq!(n.app_icon, "signal-desktop");
        assert_eq!(n.summary, "Alice sent a message");
        assert_eq!(n.body, "see you at five");
    }

    /// CRITICAL 1: the shape a GLib `GNotification` actually puts on the
    /// bus -- an empty `app_icon`, the application's app-id in
    /// `desktop-entry` and its icon in `image-path`. Measured against a
    /// stub notification server on a private `dbus-daemon`; see
    /// [`Notification`]'s doc comment for the other senders measured the
    /// same way. Decoding only `app_icon` from this leaves the settings
    /// window nothing to draw but the fallback glyph, which is what every
    /// row rendered before this.
    #[test]
    fn the_icon_hints_a_real_sender_uses_are_decoded() {
        let hints = HashMap::from([
            (
                "desktop-entry".to_string(),
                Value::from("org.gnome.Fractal"),
            ),
            ("image-path".to_string(), Value::from("mail-unread")),
            ("urgency".to_string(), Value::from(1u8)),
        ]);
        let m = notify_message_with_hints("gnotif", "", "Alice", "hi", hints);
        let Decoded::Notification(n) = decode(&m) else {
            panic!("expected a decoded notification");
        };
        assert_eq!(n.desktop_entry, "org.gnome.Fractal");
        assert_eq!(n.image_path, "mail-unread");
        assert_eq!(n.app_icon, "");
    }

    /// A hint that is absent, or present with a type the spec does not give
    /// it, is no icon rather than a decode failure: hints are optional and
    /// every sender sends a different subset, so a notification carrying an
    /// odd one is still a notification worth speaking.
    #[test]
    fn a_missing_or_mistyped_hint_is_empty_not_a_failure() {
        let hints = HashMap::from([("desktop-entry".to_string(), Value::from(7u32))]);
        let m = notify_message_with_hints("odd", "", "Alice", "hi", hints);
        let Decoded::Notification(n) = decode(&m) else {
            panic!("a mistyped hint must not make the message malformed");
        };
        assert_eq!(n.desktop_entry, "");
        assert_eq!(n.image_path, "");
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
