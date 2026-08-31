//! The event bus: sequence numbers, a replay buffer, and fan-out to clients.
//!
//! PROTOCOL.md's rules, encoded: every event carries a monotonically
//! increasing `seq`; the daemon keeps a short replay buffer so a reconnecting
//! client can ask for "everything since N" instead of re-querying; and a slow
//! client is never allowed to hold up the pipeline. That last one is why each
//! client owns a bounded outbox — when it overflows the client is disconnected,
//! because the alternative is a stalled analysis thread and lost audio.

use std::collections::{BTreeSet, VecDeque};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use tracing::{debug, warn};

/// The protocol version this daemon speaks. Bumped only on breaking changes.
pub const PROTO: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Topic {
    Segments,
    Relabel,
    Sources,
    Ops,
    Roster,
    /// Daemon state — pause above all. Not in PROTOCOL's original list; added
    /// because pause has to reach every view the instant it happens, and a
    /// three-second poll is not "instant".
    Status,
}

impl Topic {
    pub fn as_str(self) -> &'static str {
        match self {
            Topic::Segments => "segments",
            Topic::Relabel => "relabel",
            Topic::Sources => "sources",
            Topic::Ops => "ops",
            Topic::Roster => "roster",
            Topic::Status => "status",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "segments" => Topic::Segments,
            "relabel" => Topic::Relabel,
            "sources" => Topic::Sources,
            "ops" => Topic::Ops,
            "roster" => Topic::Roster,
            "status" => Topic::Status,
            _ => return None,
        })
    }

    pub const ALL: [Topic; 6] = [
        Topic::Segments,
        Topic::Relabel,
        Topic::Sources,
        Topic::Ops,
        Topic::Roster,
        Topic::Status,
    ];
}

/// One published event, kept in the replay ring exactly as it went on the wire.
#[derive(Debug, Clone)]
struct Record {
    seq: u64,
    topic: Topic,
    line: Arc<Vec<u8>>,
}

/// A connected client's write end. The reader thread parses requests; this is
/// everything the rest of the daemon needs in order to talk to it.
pub struct Client {
    pub id: u64,
    tx: SyncSender<Arc<Vec<u8>>>,
    /// A clone of the socket, kept only so a doomed client can be shut down
    /// from whichever thread noticed — that unblocks both its reader and its
    /// writer at once.
    socket: Option<UnixStream>,
    topics: Mutex<BTreeSet<Topic>>,
    dead: AtomicBool,
    dropped: AtomicU64,
}

impl Client {
    pub fn new(
        id: u64,
        socket: Option<UnixStream>,
        outbox: usize,
    ) -> (Arc<Self>, Receiver<Arc<Vec<u8>>>) {
        let (tx, rx) = sync_channel(outbox.max(1));
        (
            Arc::new(Self {
                id,
                tx,
                socket,
                topics: Mutex::new(BTreeSet::new()),
                dead: AtomicBool::new(false),
                dropped: AtomicU64::new(0),
            }),
            rx,
        )
    }

    /// Queue a line for this client. `false` means the client is gone — either
    /// it was already dead or this line did not fit, which is the same thing:
    /// a client that cannot keep up is disconnected, never waited for.
    pub fn send(&self, line: Arc<Vec<u8>>) -> bool {
        if self.is_dead() {
            return false;
        }
        match self.tx.try_send(line) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                warn!(
                    client = self.id,
                    "client outbox overflowed; disconnecting it rather than blocking"
                );
                self.kill();
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.kill();
                false
            }
        }
    }

    pub fn subscribe(&self, topics: &[Topic]) {
        let mut set = self.topics.lock().unwrap_or_else(|p| p.into_inner());
        for t in topics {
            set.insert(*t);
        }
    }

    pub fn topics(&self) -> BTreeSet<Topic> {
        self.topics
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn wants(&self, topic: Topic) -> bool {
        self.topics
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&topic)
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    /// Mark the client gone and shut its socket down so the reader and writer
    /// threads unblock and exit on their own.
    pub fn kill(&self) {
        if !self.dead.swap(true, Ordering::SeqCst)
            && let Some(sock) = &self.socket
        {
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Why a replay could not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resync;

struct Inner {
    seq: u64,
    ring: VecDeque<Record>,
    clients: Vec<Arc<Client>>,
    next_client_id: u64,
}

pub struct Bus {
    inner: Mutex<Inner>,
    replay_cap: usize,
    outbox_cap: usize,
}

impl Bus {
    pub fn new(replay_cap: usize, outbox_cap: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                seq: 0,
                ring: VecDeque::new(),
                clients: Vec::new(),
                next_client_id: 1,
            }),
            replay_cap: replay_cap.max(1),
            outbox_cap: outbox_cap.max(1),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn current_seq(&self) -> u64 {
        self.lock().seq
    }

    pub fn outbox_cap(&self) -> usize {
        self.outbox_cap
    }

    pub fn client_count(&self) -> usize {
        self.lock().clients.iter().filter(|c| !c.is_dead()).count()
    }

    /// Register a client and hand back its handle plus the receiver its writer
    /// thread drains.
    pub fn attach(&self, socket: Option<UnixStream>) -> (Arc<Client>, Receiver<Arc<Vec<u8>>>) {
        let mut inner = self.lock();
        let id = inner.next_client_id;
        inner.next_client_id += 1;
        let (client, rx) = Client::new(id, socket, self.outbox_cap);
        inner.clients.push(Arc::clone(&client));
        inner.clients.retain(|c| !c.is_dead());
        (client, rx)
    }

    pub fn detach(&self, client: &Client) {
        client.kill();
        self.lock().clients.retain(|c| c.id != client.id);
    }

    /// Hang up on everyone — the daemon is going away.
    pub fn hangup_all(&self) {
        let mut inner = self.lock();
        for client in inner.clients.drain(..) {
            client.kill();
        }
    }

    /// Assign the next sequence number, record the event, and fan it out to
    /// every subscriber. Returns the sequence number it was given.
    pub fn publish(&self, topic: Topic, ev: &str, data: Value) -> u64 {
        let mut inner = self.lock();
        inner.seq += 1;
        let seq = inner.seq;
        let line = Arc::new(encode_event(seq, ev, &data));

        inner.ring.push_back(Record {
            seq,
            topic,
            line: Arc::clone(&line),
        });
        while inner.ring.len() > self.replay_cap {
            inner.ring.pop_front();
        }

        let mut any_dead = false;
        for client in &inner.clients {
            if client.is_dead() {
                any_dead = true;
                continue;
            }
            if client.wants(topic) && !client.send(Arc::clone(&line)) {
                any_dead = true;
            }
        }
        if any_dead {
            inner.clients.retain(|c| !c.is_dead());
        }
        debug!(seq, ev, topic = topic.as_str(), "event published");
        seq
    }

    /// Everything after `since`, subject to the client's subscriptions, as a
    /// batch for the reply.
    ///
    /// It goes back in the reply rather than down the stream so the client can
    /// apply the replay *before* the live events it queued while asking —
    /// otherwise a live event bumps its position past the replay and the whole
    /// window is discarded as already-seen. Also returns the daemon's current
    /// sequence, which is what the client rebases onto.
    pub fn events_since(&self, client: &Client, since: u64) -> Result<(Vec<Value>, u64), Resync> {
        let inner = self.lock();
        if since > inner.seq {
            // The client remembers a higher sequence than we have: this daemon
            // restarted under it. Nothing here is comparable — full resync.
            return Err(Resync);
        }
        if let Some(oldest) = inner.ring.front()
            && since + 1 < oldest.seq
        {
            return Err(Resync);
        }
        let topics = client.topics();
        let events = inner
            .ring
            .iter()
            .filter(|r| r.seq > since && topics.contains(&r.topic))
            .filter_map(|r| serde_json::from_slice::<Value>(&r.line).ok())
            .collect::<Vec<_>>();
        Ok((events, inner.seq))
    }
}

/// `{"seq":N,"ev":"...","data":{...}}\n` — one event, one line.
pub fn encode_event(seq: u64, ev: &str, data: &Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&serde_json::json!({
        "seq": seq,
        "ev": ev,
        "data": data,
    }))
    .unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line_of(rx: &Receiver<Arc<Vec<u8>>>) -> Value {
        let bytes = rx.try_recv().expect("an event was expected");
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn sequence_numbers_increase_by_one_and_are_visible_before_any_client() {
        let bus = Bus::new(16, 8);
        assert_eq!(bus.current_seq(), 0);
        assert_eq!(bus.publish(Topic::Segments, "segment", json!({"id": 1})), 1);
        assert_eq!(bus.publish(Topic::Relabel, "relabel", json!({})), 2);
        assert_eq!(bus.current_seq(), 2);
    }

    #[test]
    fn a_client_only_receives_the_topics_it_subscribed_to() {
        let bus = Bus::new(16, 8);
        let (client, rx) = bus.attach(None);
        client.subscribe(&[Topic::Relabel]);

        bus.publish(Topic::Segments, "segment", json!({"id": 1}));
        bus.publish(Topic::Relabel, "relabel", json!({"speaker": 12}));

        let ev = line_of(&rx);
        assert_eq!(ev["ev"], "relabel");
        assert_eq!(ev["seq"], 2, "the seq is the bus's, not the client's count");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn two_clients_see_the_same_event_with_the_same_seq() {
        let bus = Bus::new(16, 8);
        let (a, rx_a) = bus.attach(None);
        let (b, rx_b) = bus.attach(None);
        a.subscribe(&Topic::ALL);
        b.subscribe(&Topic::ALL);

        bus.publish(Topic::Segments, "segment", json!({"id": 7}));
        assert_eq!(line_of(&rx_a), line_of(&rx_b));
        assert_eq!(bus.client_count(), 2);
    }

    #[test]
    fn a_client_that_stops_reading_is_dropped_not_waited_for() {
        // Outbox of two: the third event has nowhere to go.
        let bus = Bus::new(64, 2);
        let (slow, _rx) = bus.attach(None);
        let (fast, rx_fast) = bus.attach(None);
        slow.subscribe(&Topic::ALL);
        fast.subscribe(&Topic::ALL);

        let mut seen: Vec<u64> = Vec::new();
        for i in 0..5 {
            bus.publish(Topic::Segments, "segment", json!({"id": i}));
            // The fast client keeps up; the slow one never reads at all.
            while let Ok(b) = rx_fast.try_recv() {
                seen.push(
                    serde_json::from_slice::<Value>(&b).unwrap()["seq"]
                        .as_u64()
                        .unwrap(),
                );
            }
        }
        assert!(slow.is_dead(), "the slow client must be disconnected");
        assert!(!fast.is_dead(), "and must not have taken the other with it");
        assert_eq!(bus.client_count(), 1);
        // The fast client got every event, including the ones after the drop.
        assert_eq!(seen, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn replay_returns_everything_after_the_requested_seq_as_a_batch() {
        let bus = Bus::new(64, 16);
        for i in 0..4 {
            bus.publish(Topic::Segments, "segment", json!({"id": i}));
        }
        let (client, rx) = bus.attach(None);
        client.subscribe(&[Topic::Segments]);

        let (events, seq) = bus.events_since(&client, 2).unwrap();
        assert_eq!(seq, 4);
        assert_eq!(
            events
                .iter()
                .map(|e| e["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(
            rx.try_recv().is_err(),
            "a replay belongs in the reply, not on the stream"
        );

        // Caught up: nothing to replay, and that is not an error.
        assert_eq!(bus.events_since(&client, 4).unwrap().0.len(), 0);
    }

    #[test]
    fn replay_of_a_seq_that_fell_out_of_the_buffer_is_a_resync() {
        let bus = Bus::new(3, 16);
        for i in 0..10 {
            bus.publish(Topic::Segments, "segment", json!({"id": i}));
        }
        let (client, _rx) = bus.attach(None);
        client.subscribe(&[Topic::Segments]);

        // The ring holds 8, 9, 10; asking from 6 leaves a hole at 7.
        assert_eq!(bus.events_since(&client, 6).err(), Some(Resync));
        // Asking from 7 is exactly the boundary and is still serviceable.
        assert!(bus.events_since(&client, 7).is_ok());
        // A client remembering the future saw a different daemon.
        assert_eq!(bus.events_since(&client, 99).err(), Some(Resync));
    }

    #[test]
    fn replay_respects_the_subscription() {
        let bus = Bus::new(64, 16);
        bus.publish(Topic::Segments, "segment", json!({}));
        bus.publish(Topic::Relabel, "relabel", json!({}));
        let (client, _rx) = bus.attach(None);
        client.subscribe(&[Topic::Relabel]);

        let (events, _) = bus.events_since(&client, 0).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["ev"], "relabel");
    }

    #[test]
    fn killing_a_client_shuts_its_socket_down() {
        let (a, b) = UnixStream::pair().unwrap();
        let bus = Bus::new(8, 8);
        let (client, _rx) = bus.attach(Some(a));
        client.subscribe(&Topic::ALL);
        bus.detach(&client);
        assert!(client.is_dead());
        assert_eq!(bus.client_count(), 0);

        // The peer sees EOF rather than hanging.
        use std::io::Read;
        let mut buf = [0u8; 1];
        let mut b = b;
        assert_eq!(b.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn topics_round_trip_through_their_wire_names() {
        for t in Topic::ALL {
            assert_eq!(Topic::parse(t.as_str()), Some(t));
        }
        assert_eq!(Topic::parse("nonsense"), None);
    }
}
