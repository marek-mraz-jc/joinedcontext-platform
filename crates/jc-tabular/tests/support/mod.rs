//! The table of one whole broker answer, read the way a download reads one page (T-5632).

use jc_tabular::{Collect, Collector, Limits, Table, Window};
use serde_json::Value;

/// `entities` as one page into a [`Collector`] with no window and no byte ceiling.
pub fn table(entities: &Value, limits: &Limits) -> Result<Table, Collect> {
    let whole = Window {
        offset: 0,
        limit: usize::MAX,
    };
    let mut collector = Collector::new(*limits, whole, u64::MAX);
    collector.push_page(&serde_json::to_vec(entities).map_err(|_| Collect::Unreadable)?)?;
    Ok(collector.finish())
}
