//! Speaking desktop notifications.
//!
//! `sayd` watches the session bus rather than owning
//! `org.freedesktop.Notifications`: mako keeps receiving every notification
//! and keeps answering the applications that send them, so nothing here can
//! stop a notification being *displayed* -- only heard.
//!
//! Three layers, each testable without the one above it: `decode` turns a
//! D-Bus message into a `Notification`, `policy` decides whether and how to
//! speak it, and `monitor` owns the connection and the loop. `seen` sits
//! beside them rather than in the chain -- it does not affect whether or
//! what gets spoken, it only remembers which applications have notified and
//! their icon, for the settings window to suggest from.

pub mod decode;
pub mod monitor;
pub mod policy;
pub mod seen;

pub use decode::Notification;
