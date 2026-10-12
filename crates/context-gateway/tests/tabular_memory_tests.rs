//! A whole-dataset workbook is written without holding its sheet as text (T-4107, EP-44).
//!
//! helsinki-all's `file.xlsx` (15k entities over ~150 columns) took the dev gateway past
//! its 512Mi limit: the data sheet was built as one string and copied once more before it
//! was zipped. A counting allocator in its own test binary measures what `xlsx` holds at
//! its peak against the size of the sheet it writes.

use context_gateway::translators::tabular::{table, xlsx, Limits};
use serde_json::{json, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from `alloc` above with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Entities of many types, each with its own attributes, so the table is wide and sparse
/// the way a dataset of mixed types is.
fn mixed_answer(entities: usize) -> Value {
    Value::Array(
        (0..entities)
            .map(|n| {
                let kind = n % 10;
                let mut entity = json!({
                    "id": format!("urn:ngsi-ld:Type{kind}:helsinki:{n}"),
                    "type": format!("Type{kind}"),
                    "location": { "type": "GeoProperty",
                                  "value": { "type": "Point", "coordinates": [24.9 + n as f64 / 1e5, 60.17] } },
                });
                for attribute in 0..12 {
                    entity[format!("t{kind}a{attribute}")] = json!({
                        "type": "Property",
                        "value": format!("a value of entity {n} & attribute {attribute}"),
                    });
                }
                entity
            })
            .collect(),
    )
}

#[test]
fn a_large_workbook_never_holds_its_sheet_as_text() {
    let table = table(&mixed_answer(20_000), &Limits::DEFAULT).expect("within the limits");
    assert!(table.columns.len() > 100, "{} columns", table.columns.len());

    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let bytes = xlsx(&table, &[], &Limits::DEFAULT).expect("a workbook");
    let held = PEAK.load(Ordering::Relaxed) - before;

    let mut sheet = Vec::new();
    zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .expect("a zip")
        .by_name("xl/worksheets/sheet1.xml")
        .expect("the data sheet")
        .read_to_end(&mut sheet)
        .expect("readable");
    assert!(sheet.ends_with(b"</sheetData></worksheet>"));
    // Building the sheet as a string held it at least twice over; streamed, the gateway
    // holds the compressed file and one row.
    assert!(
        held < sheet.len() / 4,
        "xlsx held {held} bytes at its peak for a {} byte sheet",
        sheet.len()
    );
}
