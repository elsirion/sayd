//! The utterance queue and the policy that governs what a new submission does
//! to whatever is already speaking.
//!
//! `Interrupt` and `Front` differ only in what happens to the *current*
//! utterance, which this type does not own -- the engine does. Both put the
//! new utterance at the head; the engine additionally stops the current one
//! for `Interrupt`.

use std::collections::VecDeque;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Play after everything already queued.
    Enqueue,
    /// Stop the current utterance and play next. Pending entries survive.
    Interrupt,
    /// Drop everything, current and pending, and play this alone.
    Replace,
    /// Play next, but let the current utterance finish first.
    Front,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Source {
    /// The default: anything arriving over the bus behaves like agent narration.
    #[default]
    DBus,
    Cli,
    Hotkey,
    Notification,
}

impl Source {
    /// Per-source default from the spec. Agent narration over D-Bus must play
    /// in order and never talk over itself; a hotkey means "read *this* now";
    /// a notification should be timely without destroying what is playing.
    pub fn default_policy(self) -> Policy {
        match self {
            Source::DBus | Source::Cli => Policy::Enqueue,
            Source::Hotkey => Policy::Replace,
            Source::Notification => Policy::Front,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Utterance {
    pub id: u64,
    pub text: String,
    pub voice: String,
    pub speed: f32,
    pub source: Source,
}

pub struct Queue {
    items: VecDeque<Utterance>,
    next_id: u64,
}

impl Queue {
    pub fn new() -> Self {
        Queue { items: VecDeque::new(), next_id: 1 }
    }

    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Apply `policy` and add `u`. Returns the ids of any pending utterances
    /// dropped as a result; the caller is responsible for the current one.
    pub fn submit(&mut self, u: Utterance, policy: Policy) -> Vec<u64> {
        let mut dropped = Vec::new();
        match policy {
            Policy::Enqueue => self.items.push_back(u),
            Policy::Interrupt | Policy::Front => self.items.push_front(u),
            Policy::Replace => {
                dropped = self.clear();
                self.items.push_back(u);
            }
        }
        dropped
    }

    pub fn pop_front(&mut self) -> Option<Utterance> {
        self.items.pop_front()
    }

    pub fn clear(&mut self) -> Vec<u64> {
        let ids = self.items.iter().map(|u| u.id).collect();
        self.items.clear();
        ids
    }

    pub fn cancel(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|u| u.id != id);
        self.items.len() != before
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Utterance> {
        self.items.iter()
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utt(id: u64, text: &str) -> Utterance {
        Utterance {
            id,
            text: text.into(),
            voice: "af_heart".into(),
            speed: 1.0,
            source: Source::DBus,
        }
    }

    fn texts(q: &Queue) -> Vec<String> {
        q.iter().map(|u| u.text.clone()).collect()
    }

    #[test]
    fn enqueue_appends_in_order() {
        let mut q = Queue::new();
        q.submit(utt(1, "a"), Policy::Enqueue);
        q.submit(utt(2, "b"), Policy::Enqueue);
        assert_eq!(texts(&q), vec!["a", "b"]);
    }

    #[test]
    fn front_jumps_the_line_without_dropping_anything() {
        let mut q = Queue::new();
        q.submit(utt(1, "a"), Policy::Enqueue);
        q.submit(utt(2, "b"), Policy::Enqueue);
        let dropped = q.submit(utt(3, "urgent"), Policy::Front);
        assert!(dropped.is_empty());
        assert_eq!(texts(&q), vec!["urgent", "a", "b"]);
    }

    #[test]
    fn interrupt_jumps_the_line_and_keeps_pending() {
        let mut q = Queue::new();
        q.submit(utt(1, "a"), Policy::Enqueue);
        let dropped = q.submit(utt(2, "now"), Policy::Interrupt);
        assert!(dropped.is_empty(), "interrupt drops the *current* utterance, not the queue");
        assert_eq!(texts(&q), vec!["now", "a"]);
    }

    #[test]
    fn replace_clears_everything_pending() {
        let mut q = Queue::new();
        q.submit(utt(1, "a"), Policy::Enqueue);
        q.submit(utt(2, "b"), Policy::Enqueue);
        let dropped = q.submit(utt(3, "only"), Policy::Replace);
        assert_eq!(dropped, vec![1, 2]);
        assert_eq!(texts(&q), vec!["only"]);
    }

    #[test]
    fn source_defaults_match_the_spec() {
        assert_eq!(Source::DBus.default_policy(), Policy::Enqueue);
        assert_eq!(Source::Cli.default_policy(), Policy::Enqueue);
        assert_eq!(Source::Hotkey.default_policy(), Policy::Replace);
        assert_eq!(Source::Notification.default_policy(), Policy::Front);
    }

    #[test]
    fn cancel_removes_one_by_id() {
        let mut q = Queue::new();
        q.submit(utt(1, "a"), Policy::Enqueue);
        q.submit(utt(2, "b"), Policy::Enqueue);
        assert!(q.cancel(1));
        assert_eq!(texts(&q), vec!["b"]);
        assert!(!q.cancel(99), "cancelling an unknown id reports false");
    }

    #[test]
    fn clear_returns_the_dropped_ids() {
        let mut q = Queue::new();
        q.submit(utt(1, "a"), Policy::Enqueue);
        q.submit(utt(2, "b"), Policy::Enqueue);
        assert_eq!(q.clear(), vec![1, 2]);
        assert!(q.is_empty());
    }

    #[test]
    fn pop_front_drains_in_order() {
        let mut q = Queue::new();
        q.submit(utt(1, "a"), Policy::Enqueue);
        q.submit(utt(2, "b"), Policy::Enqueue);
        assert_eq!(q.pop_front().map(|u| u.text), Some("a".into()));
        assert_eq!(q.pop_front().map(|u| u.text), Some("b".into()));
        assert_eq!(q.pop_front(), None);
    }

    #[test]
    fn ids_are_unique_and_increasing() {
        let mut q = Queue::new();
        let a = q.next_id();
        let b = q.next_id();
        assert!(b > a);
    }

    #[test]
    fn default_and_new_agree_and_start_at_one() {
        let mut q_default = Queue::default();
        let mut q_new = Queue::new();
        let id_default = q_default.next_id();
        let id_new = q_new.next_id();
        assert_eq!(id_default, id_new, "Queue::default() and Queue::new() should produce same first id");
        assert_ne!(id_default, 0, "first id must not be 0 (sentinel for no utterance)");
    }
}
