//! A download's table built page by page under its ceilings (EP-44, T-0810, T-5507).
//!
//! The gateway reads the broker; this keeps what it reads: each page is parsed, cut to the
//! caller's `offset`/`limit` window, appended to the [`Table`] and dropped. Every row read counts
//! against the row ceiling, kept or not, and every byte read against the memory ceiling, so a
//! window never lets a download read more than the endpoint allows. Both refuse, never truncate.

use crate::{Limits, Table};
use serde_json::Value;

/// Why a page walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Collect {
    /// A ceiling was crossed: rows the endpoint allows, or bytes the gateway holds for one download.
    #[error("the answer is larger than this endpoint allows to be downloaded at once")]
    TooLarge,
    /// A page that is no JSON.
    #[error("a page of the answer is not JSON")]
    Unreadable,
}

/// A download parameter that is no whole number (T-4591): the caller's mistake, said, never a
/// silent default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} must be a whole number")]
pub struct BadWindow(pub &'static str);

/// The `offset` and `limit` a download asks for (T-4591); without them, the whole answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Rows skipped before the first one kept.
    pub offset: usize,
    /// Rows kept at most.
    pub limit: usize,
}

impl Window {
    /// The window of a download's query parameters; an empty value counts as absent.
    pub fn parse(params: &[(String, String)]) -> Result<Self, BadWindow> {
        let read = |name: &'static str, none: usize| match params
            .iter()
            .find(|(key, value)| key == name && !value.is_empty())
        {
            Some((_, value)) => value.parse().map_err(|_| BadWindow(name)),
            None => Ok(none),
        };
        Ok(Self {
            offset: read("offset", 0)?,
            limit: read("limit", usize::MAX)?,
        })
    }

    fn holds(&self, row: usize) -> bool {
        row.checked_sub(self.offset)
            .is_some_and(|at| at < self.limit)
    }
}

/// The entities of one broker page: an array, a single entity, or none (an empty body included).
pub fn page_entities(bytes: &[u8]) -> Result<Vec<Value>, Collect> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    Ok(
        match serde_json::from_slice(bytes).map_err(|_| Collect::Unreadable)? {
            Value::Array(entities) => entities,
            entity @ Value::Object(_) => vec![entity],
            _ => Vec::new(),
        },
    )
}

/// The table of a download, built one page at a time.
#[derive(Debug)]
pub struct Collector {
    limits: Limits,
    window: Window,
    max_held: u64,
    seen: usize,
    held: u64,
    table: Table,
}

impl Collector {
    /// An empty table for `limits` and `window`, refusing past `max_held` bytes of pages read.
    pub fn new(limits: Limits, window: Window, max_held: u64) -> Self {
        Self {
            limits,
            window,
            max_held,
            seen: 0,
            held: 0,
            table: Table::default(),
        }
    }

    /// Adds the page `bytes` hold and answers how many entities it had, kept or not.
    pub fn push_page(&mut self, bytes: &[u8]) -> Result<usize, Collect> {
        let page = page_entities(bytes)?;
        let fetched = page.len();
        let first = self.seen;
        self.seen = self.seen.saturating_add(fetched);
        self.held = self.held.saturating_add(bytes.len() as u64);
        if self.seen as u64 > u64::from(self.limits.max_rows) || self.held > self.max_held {
            return Err(Collect::TooLarge);
        }
        let kept = page
            .into_iter()
            .enumerate()
            .filter(|(index, _)| self.window.holds(first.saturating_add(*index)))
            .map(|(_, entity)| entity)
            .collect();
        self.table.append(&Value::Array(kept));
        Ok(fetched)
    }

    /// The table of every page pushed.
    pub fn finish(self) -> Table {
        self.table
    }
}
