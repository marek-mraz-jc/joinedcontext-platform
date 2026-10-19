//! An answer that outlives its page (T-4685, API/05 §1.6.2): a visitor's answer runs to its end
//! even when the reader leaves, its events are held here until 2 minutes after `done`, and the
//! widget on the next page reads them again from the index it reached. A turn belongs to the
//! address that asked (its salted daily hash) and is read with any current visitor token: the
//! token lives in the page's memory, so the next page earns a new one. Its random id is never
//! the authorization.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::watch;

use super::agent::Event;

/// How long a finished turn may still be read.
pub const RESUMABLE: Duration = Duration::from_secs(120);
/// How long after that a turn answers `410` before it is forgotten.
const GONE: Duration = Duration::from_secs(600);
/// The most turns held at once; the oldest finished goes first.
pub const MAX_TURNS: usize = 1_000;
/// The resumes one address may ask of a deployment in a minute.
pub const RESUMES_PER_MINUTE: u32 = 20;

/// The turns being answered or finished a moment ago, by id.
#[derive(Default)]
pub struct Turns {
    held: Mutex<HashMap<String, Arc<Turn>>>,
}

/// One answer's events, in the order the stream sent them.
pub struct Turn {
    public_id: String,
    visitor: String,
    log: Mutex<Log>,
    changed: watch::Sender<usize>,
}

#[derive(Default)]
struct Log {
    events: Vec<Event>,
    done: Option<Instant>,
}

/// What a resume finds under an id.
pub enum Found {
    Live(Arc<Turn>),
    Gone,
    Missing,
}

impl Turns {
    /// A new turn of `visitor` on `public_id`, with its random id.
    pub fn start(
        &self,
        public_id: &str,
        visitor: &str,
        now: Instant,
    ) -> Option<(String, Arc<Turn>)> {
        let mut bytes = [0u8; 16];
        SystemRandom::new().fill(&mut bytes).ok()?;
        let id = uuid_v4(bytes);
        let turn = Arc::new(Turn {
            public_id: public_id.to_owned(),
            visitor: visitor.to_owned(),
            log: Mutex::new(Log::default()),
            changed: watch::Sender::new(0),
        });
        let mut held = self.held.lock().ok()?;
        held.retain(|_, t| {
            t.done()
                .is_none_or(|at| now.duration_since(at) < RESUMABLE + GONE)
        });
        if held.len() >= MAX_TURNS {
            let oldest = held
                .iter()
                .filter_map(|(id, t)| t.done().map(|at| (at, id.clone())))
                .min()
                .map(|(_, id)| id);
            // ponytail: every held turn still running means MAX_QUESTIONS is past 1,000; the new
            // one is then answered without a resume.
            held.remove(&oldest?);
        }
        held.insert(id.clone(), Arc::clone(&turn));
        Some((id, turn))
    }

    /// The turn `id` as `visitor` of `public_id` may read it: another visitor's or another
    /// deployment's is missing, as an unknown one is.
    pub fn find(&self, public_id: &str, visitor: &str, id: &str, now: Instant) -> Found {
        let Some(turn) = self.held.lock().ok().and_then(|held| held.get(id).cloned()) else {
            return Found::Missing;
        };
        if turn.public_id != public_id || turn.visitor != visitor {
            return Found::Missing;
        }
        match turn.done() {
            Some(at) if now.duration_since(at) >= RESUMABLE + GONE => Found::Missing,
            Some(at) if now.duration_since(at) >= RESUMABLE => Found::Gone,
            _ => Found::Live(turn),
        }
    }
}

impl Turn {
    /// The stream's next event, kept for a resume.
    pub fn push(&self, event: &Event) {
        if let Ok(mut log) = self.log.lock() {
            log.events.push(event.clone());
        }
        self.changed.send_modify(|n| *n += 1);
    }

    /// The answer is done: no more events, and the 2 minutes start.
    pub fn finish(&self, now: Instant) {
        if let Ok(mut log) = self.log.lock() {
            log.done.get_or_insert(now);
        }
        self.changed.send_modify(|n| *n += 1);
    }

    fn done(&self) -> Option<Instant> {
        self.log.lock().ok().and_then(|log| log.done)
    }

    /// The events from index `after` on, the ones still to come as they come, until `done`.
    pub fn events_from(
        self: Arc<Self>,
        after: usize,
    ) -> impl futures_util::Stream<Item = (usize, Event)> {
        // Subscribed before the first look, so no event pushed in between is missed.
        let changes = self.changed.subscribe();
        futures_util::stream::unfold(
            (self, changes, after),
            |(turn, mut changes, index)| async move {
                loop {
                    let next = {
                        let log = turn.log.lock().ok()?;
                        match log.events.get(index) {
                            Some(event) => Some(event.clone()),
                            None if log.done.is_some() => return None,
                            None => None,
                        }
                    };
                    if let Some(event) = next {
                        return Some(((index, event), (turn, changes, index + 1)));
                    }
                    changes.changed().await.ok()?;
                }
            },
        )
    }
}

/// Sixteen random bytes as a version 4 UUID.
fn uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> Event {
        Event::Delta(text.to_owned())
    }

    #[test]
    fn an_id_is_a_version_4_uuid() {
        let id = uuid_v4([0xff; 16]);
        assert_eq!(id, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        let turns = Turns::default();
        let (a, _) = turns.start("bb", "v1", Instant::now()).expect("held");
        let (b, _) = turns.start("bb", "v1", Instant::now()).expect("held");
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }

    #[test]
    fn a_turn_is_its_visitor_s_alone_and_goes_after_its_window() {
        let turns = Turns::default();
        let now = Instant::now();
        let (id, turn) = turns.start("bb", "v1", now).expect("held");
        assert!(matches!(turns.find("bb", "v1", &id, now), Found::Live(_)));
        assert!(matches!(turns.find("bb", "v2", &id, now), Found::Missing));
        assert!(matches!(turns.find("zz", "v1", &id, now), Found::Missing));
        assert!(matches!(
            turns.find("bb", "v1", "nope", now),
            Found::Missing
        ));
        turn.finish(now);
        let later = |s| now + Duration::from_secs(s);
        assert!(matches!(
            turns.find("bb", "v1", &id, later(119)),
            Found::Live(_)
        ));
        assert!(matches!(
            turns.find("bb", "v1", &id, later(120)),
            Found::Gone
        ));
        assert!(matches!(
            turns.find("bb", "v1", &id, later(720)),
            Found::Missing
        ));
    }

    #[test]
    fn a_full_registry_drops_its_oldest_finished_turn() {
        let turns = Turns::default();
        let now = Instant::now();
        let (first, turn) = turns.start("bb", "v", now).expect("held");
        turn.finish(now);
        for i in 1..MAX_TURNS {
            let (_, t) = turns.start("bb", "v", now).expect("held");
            t.finish(now + Duration::from_millis(i as u64));
        }
        let (newest, _) = turns
            .start("bb", "v", now + Duration::from_secs(1))
            .expect("held");
        let at = now + Duration::from_secs(1);
        assert!(matches!(turns.find("bb", "v", &first, at), Found::Missing));
        assert!(matches!(turns.find("bb", "v", &newest, at), Found::Live(_)));
    }

    #[tokio::test]
    async fn events_come_from_the_index_asked_then_live_until_done() {
        use futures_util::StreamExt;
        let turns = Turns::default();
        let (_, turn) = turns.start("bb", "v", Instant::now()).expect("held");
        turn.push(&delta("a"));
        turn.push(&delta("b"));
        let reader = tokio::spawn(Arc::clone(&turn).events_from(1).collect::<Vec<_>>());
        tokio::task::yield_now().await;
        turn.push(&delta("c"));
        turn.finish(Instant::now());
        let read = reader.await.expect("read");
        assert_eq!(read, vec![(1, delta("b")), (2, delta("c"))]);
        let past = Arc::clone(&turn).events_from(9).collect::<Vec<_>>().await;
        assert!(past.is_empty());
    }
}
