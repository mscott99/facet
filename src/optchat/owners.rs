// Who a message belongs to, so a message can be sent to an instance that is running (the web's
// Alt+Enter, a Telegram reply). Kept in memory only: the engine restarts only when no instance
// runs (`restart_if_idle`), so after a restart there is nobody a message could still go to.
//
// An instance that takes messages while it runs has a *tap*: the channel instance.rs reads
// between its events. It is taken away when the instance has its result (`untap`), and a
// message to an instance with no tap is an ordinary one, starting an instance of its own,
// unless the instance is *held*: it deferred (`wait_for`, or a `pass_to` that found the other
// ended) and will start again, so what is sent to it meanwhile is kept for that start.
// Taps, holds and `send` share one lock, so a message is never sent into a tap already taken
// away, nor lost between a hold and the tap of the next start.
use super::inbox::Target;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::Sender;

/// How many log messages and inbox items are remembered: far more than a few instances at a
/// time can have answered.
const KEEP: usize = 5000;

/// A message for a running instance: its place in the log, the text sent to the model, and
/// the instance that passed it on (`pass_to`), if it came that way.
pub struct Inject { pub msg: usize, pub text: String, pub via: Option<String> }

#[derive(Default)]
pub struct Owners {
    /// log message (the user message an instance answers, one it was sent, one it said) -> instance
    by_log: BTreeMap<usize, String>,
    /// inbox item (named by time, so the oldest sorts first) -> the log message it became
    by_inbox: BTreeMap<String, usize>,
    /// the instance started last
    latest: Option<String>,
    taps: HashMap<String, Sender<Inject>>,
    /// instances that will start again: the messages sent to them since, oldest first
    held: HashMap<String, Vec<(usize, String)>>,
}

impl Owners {
    /// Log message `i` belongs to instance `inst`.
    pub fn own(&mut self, i: usize, inst: &str) {
        self.by_log.insert(i, inst.to_string());
        while self.by_log.len() > KEEP { self.by_log.pop_first(); }
    }
    /// Inbox item `id` was logged as message `i`.
    pub fn inbox(&mut self, id: &str, i: usize) {
        self.by_inbox.insert(id.to_string(), i);
        while self.by_inbox.len() > KEEP { self.by_inbox.pop_first(); }
    }
    /// An instance starts: it is the latest, and takes messages until `untap`.
    /// What was held for it and not yet taken (`take_held`) goes in first.
    pub fn tap(&mut self, inst: &str, tx: Sender<Inject>) {
        for (msg, text) in self.held.remove(inst).unwrap_or_default() { let _ = tx.send(Inject { msg, text, via: None }); }
        self.taps.insert(inst.to_string(), tx);
        self.latest = Some(inst.to_string());
    }
    /// It takes no more messages. Idempotent.
    /// It has ended: nothing is held for it either.
    pub fn untap(&mut self, inst: &str) { self.taps.remove(inst); self.held.remove(inst); }
    /// It takes no messages now but will start again: those sent meanwhile are kept for it.
    pub fn hold(&mut self, inst: &str) { self.taps.remove(inst); self.held.entry(inst.to_string()).or_default(); }
    /// It is starting again: what was kept for it so far (it stays held until its tap).
    pub fn take_held(&mut self, inst: &str) -> Vec<(usize, String)> { self.held.get_mut(inst).map(std::mem::take).unwrap_or_default() }
    /// The tap of `inst`, if it still takes messages.
    pub fn tap_of(&self, inst: &str) -> Option<&Sender<Inject>> { self.taps.get(inst) }

    /// The running instance a message is for, if any: nothing when it names none, or one that
    /// has ended (never the latest instead: an older instance's thread is not for it).
    /// A held instance counts: the message waits for its next start.
    pub fn resolve(&self, to: &Target) -> Option<String> {
        let inst = match to {
            Target::Latest => self.latest.as_deref()?,
            Target::Log { i } => self.by_log.get(i)?,
            Target::Inbox { id } => self.by_log.get(self.by_inbox.get(id)?)?,
        };
        (self.taps.contains_key(inst) || self.held.contains_key(inst)).then(|| inst.to_string())
    }
    /// Message `msg` goes to `inst` (as `resolve` found it): into its tap, or held for its start.
    pub fn send(&mut self, inst: &str, msg: usize, text: String) {
        if let Some(tap) = self.taps.get(inst) { let _ = tap.send(Inject { msg, text, via: None }); }
        else if let Some(h) = self.held.get_mut(inst) { h.push((msg, text)); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn a_target_finds_only_a_running_instance() {
        let mut o = Owners::default();
        let (a, b) = (channel().0, channel().0);
        o.tap("inst_a", a);
        o.own(10, "inst_a");
        o.inbox("in-1", 9);
        o.own(9, "inst_a");
        o.tap("inst_b", b);
        o.own(20, "inst_b");
        fn name(o: &Owners, t: Target) -> Option<String> { o.resolve(&t) }
        // the latest is the one started last, not the one that spoke last
        assert_eq!(name(&o, Target::Latest).as_deref(), Some("inst_b"));
        assert_eq!(name(&o, Target::Log { i: 10 }).as_deref(), Some("inst_a"));
        assert_eq!(name(&o, Target::Inbox { id: "in-1".into() }).as_deref(), Some("inst_a"));
        assert_eq!(name(&o, Target::Log { i: 11 }), None);
        assert_eq!(name(&o, Target::Inbox { id: "nope".into() }), None);
        // ended: a message to it is an ordinary one, whatever else runs
        o.untap("inst_a");
        o.untap("inst_a");
        assert_eq!(name(&o, Target::Log { i: 10 }), None);
        assert_eq!(name(&o, Target::Inbox { id: "in-1".into() }), None);
        o.untap("inst_b");
        assert_eq!(name(&o, Target::Latest), None);
    }

    #[test]
    fn a_held_instance_gets_its_messages_at_its_next_start() {
        let mut o = Owners::default();
        let (tx, _rx) = channel();
        o.tap("inst_a", tx);
        o.own(10, "inst_a");
        o.hold("inst_a");
        assert_eq!(o.resolve(&Target::Log { i: 10 }).as_deref(), Some("inst_a"));
        o.send("inst_a", 11, "one".into());
        assert_eq!(o.take_held("inst_a"), vec![(11, "one".to_string())]);
        // still held while it starts: a later one goes in through its tap
        o.send("inst_a", 12, "two".into());
        let (tx, rx) = channel();
        o.tap("inst_a", tx);
        let got: Vec<usize> = rx.try_iter().map(|m| m.msg).collect();
        assert_eq!(got, vec![12]);
        assert!(o.take_held("inst_a").is_empty());
        // ended: nothing held, a message to it is an ordinary one
        o.hold("inst_a");
        o.untap("inst_a");
        assert_eq!(o.resolve(&Target::Log { i: 10 }), None);
    }

    #[test]
    fn it_forgets_the_oldest() {
        let mut o = Owners::default();
        for i in 0..KEEP + 10 { o.own(i, "x"); o.inbox(&format!("{:08}", i), i); }
        assert_eq!(o.by_log.len(), KEEP);
        assert_eq!(o.by_log.keys().next(), Some(&10));
        assert_eq!(o.by_inbox.len(), KEEP);
        assert_eq!(o.by_inbox.keys().next().map(String::as_str), Some("00000010"));
    }
}
