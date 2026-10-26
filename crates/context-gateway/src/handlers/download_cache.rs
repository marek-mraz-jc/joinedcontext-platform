//! What a `file.csv` download reuses for [`FRESH_FOR`] (T-5629): the anonymous answer of each
//! Endpoint and query, built once however many callers ask at the same time, and the type list
//! of each space the gateway asks the broker for on its own account.
//!
//! On dev the helsinki-air-quality file (3 KB) took 4-22 s, nearly all of it the broker's
//! space-wide `/ngsi-ld/v1/types`, and the third of three parallel downloads met the
//! [`MAX_DOWNLOADS`](crate::middleware::in_flight::MAX_DOWNLOADS) ceiling as a `503`. Only the
//! anonymous caller's answer is kept: every anonymous caller of an Endpoint is the one same
//! [`Subject`], so the view answers each of them the same rows, and a caller with a token is
//! built afresh and never served from here.

use crate::auth::subject::{anonymous_on, Subject};
use crate::middleware::response::asked_about_narrowing;
use crate::resolver::Endpoint;
use axum::body::Bytes;
use axum::extract::Request;
use axum::http::header::LINK;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio::time::Instant;

/// How long a built answer or a type list is served without asking the broker again.
pub const FRESH_FOR: Duration = Duration::from_secs(30);
/// A build nobody finished (its caller went away) stops holding its key after this long.
const ABANDONED_AFTER: Duration = Duration::from_secs(120);
/// The answers held at once; one more key is built and not kept.
const MAX_HELD: usize = 64;
/// The largest answer kept, so the cache holds at most `MAX_HELD` MiB: a larger file is built
/// for each caller, as before.
const MAX_HELD_BYTES: usize = 1024 * 1024;

/// Everything of a request the view's answer to an anonymous caller depends on besides the data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    slug: String,
    query: String,
    link: Option<Vec<u8>>,
    narrowing: bool,
}

impl Key {
    /// The key of `request`, or `None` for any caller but the Endpoint's anonymous one.
    pub(crate) fn of(endpoint: &Endpoint, subject: &Subject, request: &Request) -> Option<Self> {
        (*subject == anonymous_on(endpoint)).then(|| Self {
            slug: endpoint.slug.clone(),
            query: request.uri().query().unwrap_or_default().to_owned(),
            link: request.headers().get(LINK).map(|v| v.as_bytes().to_vec()),
            narrowing: asked_about_narrowing(request),
        })
    }
}

/// One built answer: the file and whether the view narrowed it.
#[derive(Clone, Debug)]
struct Built {
    bytes: Bytes,
    restricted: bool,
    at: Instant,
}

struct Entry {
    born: Instant,
    built: OnceCell<Built>,
}

impl Entry {
    fn fresh(&self) -> bool {
        match self.built.get() {
            Some(built) => built.at.elapsed() < FRESH_FOR,
            None => self.born.elapsed() < ABANDONED_AFTER,
        }
    }
}

/// The anonymous `file.csv` answers.
#[derive(Default)]
pub(crate) struct Downloads {
    held: Mutex<HashMap<Key, Arc<Entry>>>,
}

impl Downloads {
    /// The fresh answer under `key`, else the one `build` makes; with no key, `build`'s alone.
    /// Callers asking while it builds wait for it instead of building their own; a build that
    /// fails is that caller's answer alone, and the next caller builds again.
    pub(crate) async fn get_or_build<E, F>(
        &self,
        key: Option<Key>,
        build: F,
    ) -> Result<(Bytes, bool), E>
    where
        F: Future<Output = Result<(Bytes, bool), E>>,
    {
        let Some(key) = key else {
            return build.await;
        };
        let entry = self.entry(&key);
        let made = entry
            .built
            .get_or_try_init(|| async {
                let (bytes, restricted) = build.await?;
                Ok(Built {
                    bytes,
                    restricted,
                    at: Instant::now(),
                })
            })
            .await
            .cloned();
        if made
            .as_ref()
            .map_or(true, |built| built.bytes.len() > MAX_HELD_BYTES)
        {
            self.forget(&key, &entry);
        }
        made.map(|built| (built.bytes, built.restricted))
    }

    fn entry(&self, key: &Key) -> Arc<Entry> {
        let mut held = lock(&self.held);
        held.retain(|_, entry| entry.fresh());
        if let Some(entry) = held.get(key) {
            return Arc::clone(entry);
        }
        let entry = Arc::new(Entry {
            born: Instant::now(),
            built: OnceCell::new(),
        });
        if held.len() < MAX_HELD {
            held.insert(key.clone(), Arc::clone(&entry));
        }
        entry
    }

    fn forget(&self, key: &Key, entry: &Arc<Entry>) {
        let mut held = lock(&self.held);
        if held.get(key).is_some_and(|kept| Arc::ptr_eq(kept, entry)) {
            held.remove(key);
        }
    }
}

/// A space's type list and when the broker reported it.
type HeldList = (Instant, Arc<Vec<String>>);

/// The type list of each space as the broker last reported it to the gateway.
#[derive(Default)]
pub(crate) struct HeldTypes {
    held: Mutex<HashMap<String, HeldList>>,
}

impl HeldTypes {
    /// The fresh list of `space`, if one is held.
    pub(crate) fn get(&self, space: &str) -> Option<Arc<Vec<String>>> {
        let held = lock(&self.held);
        let (at, types) = held.get(space)?;
        (at.elapsed() < FRESH_FOR).then(|| Arc::clone(types))
    }

    /// Holds `types` as the list of `space` from now on.
    pub(crate) fn put(&self, space: &str, types: Vec<String>) -> Arc<Vec<String>> {
        let types = Arc::new(types);
        let mut held = lock(&self.held);
        held.retain(|_, (at, _)| at.elapsed() < FRESH_FOR);
        held.insert(space.to_owned(), (Instant::now(), Arc::clone(&types)));
        types
    }
}

/// The public catalogue records per profile and project (F-PLT1-012): rendering them asks the
/// broker for each public Endpoint's schema index, so a harvester polling the feed cost one hop
/// per dataset per request. A record is reused for [`FRESH_FOR`] while the Endpoint table it was
/// rendered from is the one the resolver holds; a reload renders afresh.
#[derive(Default)]
pub(crate) struct CatalogRecords {
    held: Mutex<HashMap<String, HeldRecords>>,
}

type EndpointTable = Arc<HashMap<String, Arc<Endpoint>>>;
type HeldRecords = (Instant, EndpointTable, Arc<Vec<serde_json::Value>>);

impl CatalogRecords {
    /// The records held under `key`, rendered from `table` less than [`FRESH_FOR`] ago.
    pub(crate) fn get(
        &self,
        key: &str,
        table: &EndpointTable,
    ) -> Option<Arc<Vec<serde_json::Value>>> {
        let held = lock(&self.held);
        let (at, from, records) = held.get(key)?;
        (at.elapsed() < FRESH_FOR && Arc::ptr_eq(from, table)).then(|| Arc::clone(records))
    }

    /// Holds `records`, rendered from `table`, under `key` from now on.
    pub(crate) fn put(
        &self,
        key: &str,
        table: EndpointTable,
        records: Vec<serde_json::Value>,
    ) -> Arc<Vec<serde_json::Value>> {
        let records = Arc::new(records);
        let mut held = lock(&self.held);
        held.retain(|_, (at, _, _)| at.elapsed() < FRESH_FOR);
        held.insert(
            key.to_owned(),
            (Instant::now(), table, Arc::clone(&records)),
        );
        records
    }
}

/// A poisoned lock still holds a consistent map: every write to it is one insert or remove.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(query: &str) -> Option<Key> {
        Some(Key {
            slug: "s".to_owned(),
            query: query.to_owned(),
            link: None,
            narrowing: false,
        })
    }

    async fn counted(builds: &AtomicUsize, size: usize) -> Result<(Bytes, bool), ()> {
        builds.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok((Bytes::from(vec![b'x'; size]), false))
    }

    #[tokio::test]
    async fn callers_at_once_share_one_build() {
        let downloads = Downloads::default();
        let builds = AtomicUsize::new(0);
        let (a, b, c) = tokio::join!(
            downloads.get_or_build(key("q"), counted(&builds, 3)),
            downloads.get_or_build(key("q"), counted(&builds, 3)),
            downloads.get_or_build(key("q"), counted(&builds, 3)),
        );
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(a, Ok((Bytes::from_static(b"xxx"), false)));
        assert_eq!(b.map(|(bytes, _)| bytes.len()), Ok(3));
        assert_eq!(c.map(|(bytes, _)| bytes.len()), Ok(3));
    }

    #[tokio::test]
    async fn another_query_and_a_failure_are_built_again() {
        let downloads = Downloads::default();
        let builds = AtomicUsize::new(0);
        let failed = downloads
            .get_or_build(key("q"), async { Err("down") })
            .await;
        assert_eq!(failed, Err("down"));
        let _ = downloads.get_or_build(key("q"), counted(&builds, 1)).await;
        let _ = downloads.get_or_build(key("q"), counted(&builds, 1)).await;
        let _ = downloads
            .get_or_build(key("other"), counted(&builds, 1))
            .await;
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_large_file_is_not_kept() {
        let downloads = Downloads::default();
        let builds = AtomicUsize::new(0);
        for _ in 0..2 {
            let built = downloads
                .get_or_build(key("q"), counted(&builds, MAX_HELD_BYTES + 1))
                .await;
            assert_eq!(built.map(|(bytes, _)| bytes.len()), Ok(MAX_HELD_BYTES + 1));
        }
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_goes_stale() {
        let downloads = Downloads::default();
        let builds = AtomicUsize::new(0);
        let _ = downloads.get_or_build(key("q"), counted(&builds, 1)).await;
        tokio::time::advance(FRESH_FOR).await;
        let _ = downloads.get_or_build(key("q"), counted(&builds, 1)).await;
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn only_the_anonymous_caller_has_a_key() {
        let endpoint = crate::fanout::hub::tests_support::endpoint();
        let request = Request::builder()
            .uri("/api/endpoint/s/file.csv?type=A")
            .body(axum::body::Body::empty())
            .expect("a request");
        let anonymous = anonymous_on(&endpoint);
        assert!(Key::of(&endpoint, &anonymous, &request).is_some());
        let person = Subject {
            user: Some("anna".to_owned()),
            ..anonymous.clone()
        };
        let account = Subject {
            service_account: Some("app".to_owned()),
            ..anonymous.clone()
        };
        let participant = Subject {
            did: Some("did:web:x".to_owned()),
            ..anonymous.clone()
        };
        let mut steward = anonymous.clone();
        steward.roles.insert("steward".to_owned());
        for caller in [person, account, participant, steward] {
            assert_eq!(Key::of(&endpoint, &caller, &request), None, "{caller:?}");
        }
    }

    #[test]
    fn a_type_list_is_held_per_space() {
        let records = CatalogRecords::default();
        let table: EndpointTable = Arc::default();
        assert_eq!(records.get("dcat/", &table), None);
        records.put(
            "dcat/",
            Arc::clone(&table),
            vec![serde_json::json!({"a": 1})],
        );
        assert_eq!(records.get("dcat/", &table).map(|r| r.len()), Some(1));
        let reloaded: EndpointTable = Arc::default();
        assert_eq!(
            records.get("dcat/", &reloaded),
            None,
            "a reload renders afresh"
        );

        let types = HeldTypes::default();
        assert_eq!(types.get("a"), None);
        types.put("a", vec!["T".to_owned()]);
        assert_eq!(types.get("a").as_deref(), Some(&vec!["T".to_owned()]));
        assert_eq!(types.get("b"), None);
    }
}
