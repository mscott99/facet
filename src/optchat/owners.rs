// Who a message belongs to, so a message can be sent to an instance that is running (the web's
// Alt+Enter, a Telegram reply). Kept in memory only: the engine restarts only when no instance
// runs (`restart_if_idle`), so after a restart there is nobody a message could still go to.
//
// An instance that takes messages while it runs has a *tap*: the channel instance.rs reads
// between its events. It is taken away when the instance has its result (`untap`), and a
// message to an instance with no tap is an ordinary one, starting an instance of its own.
// Taps and `send` share one lock, so a message is never sent into a tap already taken away.
use super::inbox::Target;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::Sender;

/// How many log messages and inbox items are remembered: far more than a few instances at a
/// time can have answered.
const KEEP: usize = 5000;

/// A message for a running instance: its place in the log, and the text sent to the model.
pub struct Inject { pub msg: usize, pub text: String }

#[derive(Default)]
pub struct Owners {
    /// log message (the user message an instance answers, one it was sent, one it said) -> instance
    by_log: BTreeMap<usize, String>,
    /// inbox item (named by time, so the oldest sorts first) -> the log message it became
    by_inbox: BTreeMap<String, usize>,
    /// the instance started last
    latest: Option<String>,
    taps: HashMap<String, Sender<Inject>>,
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
    pub fn tap(&mut self, inst: &str, tx: Sender<Inject>) {
        self.taps.insert(inst.to_string(), tx);
        self.latest = Some(inst.to_string());
    }
    /// It takes no more messages. Idempotent.
    pub fn untap(&mut self, inst: &str) { self.taps.remove(inst); }

    /// The running instance a message is for, if any: nothing when it names none, or one that
    /// has ended (never the latest instead: an older instance's thread is not for it).
    pub fn resolve(&self, to: &Target) -> Option<(&str, &Sender<Inject>)> {
        let inst = match to {
            Target::Latest => self.latest.as_deref()?,
            Target::Log { i } => self.by_log.get(i)?,
            Target::Inbox { id } => self.by_log.get(self.by_inbox.get(id)?)?,
        };
        self.taps.get(inst).map(|t| (inst, t))
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
        fn name(o: &Owners, t: Target) -> Option<String> { o.resolve(&t).map(|r| r.0.to_string()) }
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
    fn it_forgets_the_oldest() {
        let mut o = Owners::default();
        for i in 0..KEEP + 10 { o.own(i, "x"); o.inbox(&format!("{:08}", i), i); }
        assert_eq!(o.by_log.len(), KEEP);
        assert_eq!(o.by_log.keys().next(), Some(&10));
        assert_eq!(o.by_inbox.len(), KEEP);
        assert_eq!(o.by_inbox.keys().next().map(String::as_str), Some("00000010"));
    }
}
