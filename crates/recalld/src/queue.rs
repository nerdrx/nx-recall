//! Bounded hand-off between the PipeWire thread and the inference thread.
//!
//! The capture side must never block and must never grow without limit: a
//! stalled VAD thread has to cost bounded memory, and the audio worth keeping
//! is the *newest* audio, so an overflow drops the oldest buffer rather than
//! refusing the new one. Control events (session end) are never dropped —
//! losing one would leak a session row and strand a half-written segment.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// One buffer of 16 kHz mono audio, as captured.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub session_id: i64,
    /// `CLOCK_MONOTONIC` at the moment this buffer left PipeWire, i.e. the
    /// instant of its first sample.
    pub capture_mono_ns: u64,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone)]
pub enum CaptureEvent {
    Audio(AudioChunk),
    /// The source's node went away (app quit, device unplugged); flush and close.
    SessionEnd {
        session_id: i64,
        mono_ns: u64,
    },
    /// A flap was absorbed (0.13.0, `crate::flap`): the source's node vanished
    /// and reappeared inside the grace window, and `capture.rs` kept the SAME
    /// session id rather than closing it. `mono_ns` is the monotonic stamp of
    /// the reappearance (what the resumed stream's first buffer will be
    /// measured against) and `gap_ms` is how long the audio was missing.
    /// Never dropped by the queue's overflow policy, for the same reason
    /// `SessionEnd` is not: losing one would leave the pipeline's clock
    /// anchor stale and the open turn spliced across a hole it never knew
    /// about.
    Gap {
        session_id: i64,
        mono_ns: u64,
        gap_ms: u64,
    },
}

impl CaptureEvent {
    /// Weight against the queue's capacity. Only audio counts.
    fn weight(&self) -> usize {
        match self {
            CaptureEvent::Audio(c) => c.samples.len(),
            CaptureEvent::SessionEnd { .. } | CaptureEvent::Gap { .. } => 0,
        }
    }

    fn is_audio(&self) -> bool {
        matches!(self, CaptureEvent::Audio(_))
    }
}

struct Inner {
    items: VecDeque<CaptureEvent>,
    queued_samples: usize,
    closed: bool,
}

/// Multi-producer / single-consumer queue with a drop-oldest overflow policy.
pub struct EventQueue {
    inner: Mutex<Inner>,
    not_empty: Condvar,
    capacity_samples: usize,
    dropped_samples: AtomicU64,
    dropped_chunks: AtomicU64,
}

impl EventQueue {
    /// `capacity_samples` is the audio budget; control events are exempt.
    pub fn new(capacity_samples: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                items: VecDeque::new(),
                queued_samples: 0,
                closed: false,
            }),
            not_empty: Condvar::new(),
            capacity_samples: capacity_samples.max(1),
            dropped_samples: AtomicU64::new(0),
            dropped_chunks: AtomicU64::new(0),
        })
    }

    pub fn for_seconds(seconds: f32, rate: u32) -> Arc<Self> {
        Self::new((seconds.max(0.1) * rate as f32) as usize)
    }

    /// Enqueue, evicting the oldest audio if that pushes us over capacity.
    ///
    /// Returns the number of chunks evicted by this push.
    pub fn push(&self, event: CaptureEvent) -> usize {
        let mut evicted = 0usize;
        {
            let mut inner = match self.inner.lock() {
                Ok(g) => g,
                // A poisoned queue means the consumer panicked; recover the
                // guard rather than propagating a panic into the audio thread.
                Err(p) => p.into_inner(),
            };
            if inner.closed {
                return 0;
            }
            let w = event.weight();
            inner.queued_samples += w;
            inner.items.push_back(event);

            while inner.queued_samples > self.capacity_samples {
                // Remove the oldest *audio* item, stepping over control events
                // so their ordering relative to surviving audio is preserved.
                let (oldest, has_more) = {
                    let mut audio = inner
                        .items
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.is_audio())
                        .map(|(i, _)| i);
                    (audio.next(), audio.next().is_some())
                };
                // Never evict the last remaining buffer: a single chunk larger
                // than the whole budget is still the newest audio we have.
                let (Some(idx), true) = (oldest, has_more) else {
                    break;
                };
                let Some(victim) = inner.items.remove(idx) else {
                    break;
                };
                let vw = victim.weight();
                inner.queued_samples = inner.queued_samples.saturating_sub(vw);
                self.dropped_samples.fetch_add(vw as u64, Ordering::Relaxed);
                self.dropped_chunks.fetch_add(1, Ordering::Relaxed);
                evicted += 1;
            }
        }
        self.not_empty.notify_one();
        evicted
    }

    /// Block until an event is available. `None` once closed and drained.
    pub fn pop(&self) -> Option<CaptureEvent> {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        loop {
            if let Some(item) = inner.items.pop_front() {
                inner.queued_samples = inner.queued_samples.saturating_sub(item.weight());
                return Some(item);
            }
            if inner.closed {
                return None;
            }
            inner = match self.not_empty.wait(inner) {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
        }
    }

    /// Wake the consumer and let it finish; further pushes are ignored.
    pub fn close(&self) {
        {
            let mut inner = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            inner.closed = true;
        }
        self.not_empty.notify_all();
    }

    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples.load(Ordering::Relaxed)
    }

    pub fn dropped_chunks(&self) -> u64 {
        self.dropped_chunks.load(Ordering::Relaxed)
    }

    pub fn queued_samples(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.queued_samples,
            Err(p) => p.into_inner().queued_samples,
        }
    }

    pub fn capacity_samples(&self) -> usize {
        self.capacity_samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(session_id: i64, n: usize, mark: f32) -> CaptureEvent {
        CaptureEvent::Audio(AudioChunk {
            session_id,
            capture_mono_ns: 0,
            samples: vec![mark; n],
        })
    }

    fn mark_of(e: &CaptureEvent) -> f32 {
        match e {
            CaptureEvent::Audio(c) => c.samples[0],
            _ => f32::NAN,
        }
    }

    #[test]
    fn under_capacity_nothing_is_dropped() {
        let q = EventQueue::new(100);
        for i in 0..5 {
            assert_eq!(q.push(chunk(1, 10, i as f32)), 0);
        }
        assert_eq!(q.dropped_chunks(), 0);
        assert_eq!(q.queued_samples(), 50);
        for i in 0..5 {
            assert_eq!(mark_of(&q.pop().unwrap()), i as f32);
        }
        assert_eq!(q.queued_samples(), 0);
    }

    #[test]
    fn overflow_drops_oldest_and_counts_it() {
        let q = EventQueue::new(30);
        for i in 0..5 {
            q.push(chunk(1, 10, i as f32));
        }
        // Capacity 30 samples = 3 chunks of 10; the first two are evicted.
        assert_eq!(q.dropped_chunks(), 2);
        assert_eq!(q.dropped_samples(), 20);
        assert_eq!(q.queued_samples(), 30);

        let survivors: Vec<f32> = std::iter::from_fn(|| q.pop())
            .map(|e| mark_of(&e))
            .take(3)
            .collect();
        assert_eq!(survivors, vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn a_single_oversized_chunk_is_kept() {
        // Nothing else is in flight, so evicting the only item would lose the
        // newest audio — exactly what the policy is meant to preserve.
        let q = EventQueue::new(10);
        q.push(chunk(1, 100, 7.0));
        assert_eq!(q.dropped_chunks(), 0);
        assert_eq!(mark_of(&q.pop().unwrap()), 7.0);
    }

    #[test]
    fn control_events_survive_overflow_and_keep_their_order() {
        let q = EventQueue::new(20);
        q.push(chunk(1, 10, 0.0));
        q.push(CaptureEvent::SessionEnd {
            session_id: 1,
            mono_ns: 42,
        });
        q.push(chunk(2, 10, 1.0));
        q.push(chunk(2, 10, 2.0));
        // 30 samples of audio against a 20-sample budget: chunk 0 goes.
        assert_eq!(q.dropped_chunks(), 1);

        match q.pop().unwrap() {
            CaptureEvent::SessionEnd {
                session_id,
                mono_ns,
            } => {
                assert_eq!(session_id, 1);
                assert_eq!(mono_ns, 42);
            }
            other => panic!("expected SessionEnd first, got {other:?}"),
        }
        assert_eq!(mark_of(&q.pop().unwrap()), 1.0);
        assert_eq!(mark_of(&q.pop().unwrap()), 2.0);
    }

    #[test]
    fn close_drains_then_returns_none() {
        let q = EventQueue::new(100);
        q.push(chunk(1, 5, 0.0));
        q.close();
        assert!(q.pop().is_some());
        assert!(q.pop().is_none());
        // Pushes after close are ignored rather than resurrecting the queue.
        q.push(chunk(1, 5, 1.0));
        assert!(q.pop().is_none());
    }

    #[test]
    fn pop_blocks_until_a_producer_pushes() {
        let q = EventQueue::new(100);
        let producer = Arc::clone(&q);
        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            producer.push(chunk(9, 4, 3.0));
        });
        let got = q.pop().expect("blocked pop should return the pushed chunk");
        assert_eq!(mark_of(&got), 3.0);
        h.join().unwrap();
    }
}
