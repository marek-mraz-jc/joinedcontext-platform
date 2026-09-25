//! The heap a DataStore mirror needs at its peak (T-2968, EP-65, OPS-27).
//!
//! The Portal reloads every mirrored table on every reconcile pass, inside its own process, so
//! what one table costs at its peak is memory the Portal's limit has to hold. The largest tables
//! on dev are praha's 9813 WasteContainerIsle rows and bbsk-kraj's 7742: this measures a table of
//! that size against a catalogue that keeps nothing, with an allocator that counts.
//!
//! Its own test binary: the counting allocator is global to the binary it is linked into.

mod common;

use jcctl::commands::publish_ckan::{publish_one, targets};
use jcctl::loader::Repository;
use jcctl::publish::ckan::{CkanApi, CkanError, InMemoryCkan, Settings};
use serde_json::{json, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the layout is the caller's, passed through unchanged.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `alloc` above with this layout.
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One measurement at a time: the counters are global.
static MEASURING: Mutex<()> = Mutex::new(());

/// The bytes allocated above what was live when `work` started, at its highest.
fn peak_of<T>(work: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = work();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

/// A catalogue that answers like CKAN and keeps no row: what is measured is the publisher.
struct Forgetful(InMemoryCkan);

impl CkanApi for Forgetful {
    fn show(&self, action: &str, name: &str) -> Result<Option<Value>, CkanError> {
        self.0.show(action, name)
    }

    fn action(&mut self, action: &str, payload: &Value) -> Result<Value, CkanError> {
        if action == "datastore_upsert" {
            return Ok(json!({}));
        }
        self.0.action(action, payload)
    }
}

const INSTANCE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: CkanInstance
metadata:
  name: open-data
  namespace: ovzdusie
spec:
  url: https://data.example.org
  organizationDefault: mesto
  apiTokenRef: { name: ckan-open-data, key: apiToken, envVar: CKAN_OPEN_DATA_TOKEN }
"#;

const ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: waste-rows
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: k4y7pq2mzt6vhx3nbwrs5cjd3f
  audience: public
  enabledRepresentations: [csv]
  publish:
    ckan:
      instanceRef: { kind: CkanInstance, name: open-data }
      datastore: { representation: csv, refresh: onReconcile }
"#;

/// A CSV as the gateway writes it for a WasteContainerIsle-like table: an id, a type, a quoted
/// GeoJSON point, an address, numbers and short texts, `rows` lines of it.
fn table(rows: usize) -> String {
    let mut text = String::from(
        "id,type,location.value,address.value,name.value,district.value,containerCount.value,\
         fillingLevel.value,wasteType.value,accessibility.value,owner.value,dateModified.value\r\n",
    );
    for row in 0..rows {
        text.push_str(&format!(
            "urn:ngsi-ld:WasteContainerIsle:praha.eu:odpady:{row},WasteContainerIsle,\
             \"{{\"\"type\"\":\"\"Point\"\",\"\"coordinates\"\":[14.{row:05},50.{row:05}]}}\",\
             \"Ulice {row}, Praha 4\",Stanoviště {row},Praha {d},{c},0.{f:02},\
             papír,volně,Magistrát hl. m. Prahy,2026-09-25T12:00:00Z\r\n",
            d = row % 22 + 1,
            c = row % 7 + 1,
            f = row % 100,
        ));
    }
    text
}

/// T-2968: a 9813-row table is published within a bounded heap. The bound is what the table
/// itself is, a few times over; it is not a copy of the table per representation of a row.
#[test]
fn a_large_mirrored_table_is_published_within_a_bounded_heap() {
    let _one = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    let dir = common::demo_repo("mirror-peak");
    common::write(&dir, "projects/ovzdusie/ckan/open-data.yaml", INSTANCE);
    common::write(
        &dir,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/waste-rows.yaml",
        ENDPOINT,
    );
    let repo = Repository::load(&dir).expect("the repository loads");
    let target = targets(&repo, "ovzdusie")
        .expect("the walk")
        .into_iter()
        .next()
        .expect("one target");
    let csv = table(9813);
    let record =
        json!({ "@type": "dcat:Dataset", "dct:title": [{ "@value": "Waste", "@language": "en" }] });
    let mut api = Forgetful(InMemoryCkan::new().with_organization("mesto"));

    let (line, peak) = peak_of(|| {
        publish_one(
            &mut api,
            &target,
            &record,
            Some(&csv),
            &Settings::new("data.example.org"),
        )
    });
    let line = line.expect("the table is published");
    assert_eq!(line.mirror.map(|m| m.rows), Some(9813));
    eprintln!(
        "csv {} bytes, peak {} bytes ({:.1}x the csv)",
        csv.len(),
        peak,
        peak as f64 / csv.len() as f64
    );
    assert!(
        peak <= 5 * csv.len(),
        "a {}-byte table peaked at {peak} bytes",
        csv.len()
    );
}
