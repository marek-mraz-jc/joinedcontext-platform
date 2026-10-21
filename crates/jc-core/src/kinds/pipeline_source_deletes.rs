//! `spec.sourceDeletes` of a Pipeline (PL-90…PL-94): how the pipeline follows a record its source
//! removed. `never` keeps today's behaviour; `snapshot` deletes what complete runs no longer read;
//! `events` deletes what the mapped record marks as deleted; `lease` relies on a rolling
//! `spec.lifetime`. Only entities in the pipeline's own source ledger are ever candidates; the
//! ledger, the guards and the deletes are the runner's (T-4891).
//!
//! The checks here read the Pipeline alone; the one that needs its DataSource (a snapshot of a
//! source that never ends) is in `data_source::check_class`; the
//! author-Bloblang rules on `deletedWhen` (PL-50) are applied by `PipelineSpec::validate`.

use crate::error::{Error, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Consecutive complete runs an id must be missing from before it is deleted, by default (PL-91).
pub const MISSES_DEFAULT: u8 = 2;
/// The most consecutive misses a snapshot may ask for (PL-90).
pub const MISSES_MAX: u8 = 10;
/// The share of the ledger, in percent, one run may delete before it is held, by default (PL-91).
pub const MAX_SHARE_DEFAULT: u8 = 10;

/// How a pipeline follows deletions at its source (PL-90).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SourceDeleteMode {
    /// Nothing is deleted when the source drops a record; said so in the editor.
    Never,
    /// An id missing from `misses` consecutive complete runs is deleted, within `maxShare`.
    Snapshot,
    /// A mapped record for which `deletedWhen` holds deletes the entity of its id.
    Events,
    /// The entity ends by a rolling `spec.lifetime`, renewed by each write.
    Lease,
}

/// `spec.sourceDeletes` (PL-90).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SourceDeletes {
    /// `never`, `snapshot`, `events` or `lease`.
    pub mode: SourceDeleteMode,
    /// `snapshot`: consecutive complete runs an id is missing from before it goes, 1 to 10
    /// (default 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub misses: Option<u8>,
    /// `snapshot`: the most of the ledger, in percent (1 to 100, default 10), one run deletes;
    /// a run that would delete more deletes nothing and is held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_share: Option<u8>,
    /// `events`: a Bloblang query on the mapped record, true when the source says the record
    /// is deleted (for example `this.status.value == "withdrawn"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_when: Option<String>,
}

/// What the Pipeline knows that a source-delete rule depends on.
pub struct Context {
    /// The pipeline names a `period` or a `schedule`: it runs, so a run can end complete.
    pub has_clock: bool,
    /// The pipeline carries `spec.expiry`, the age sweep (PL-64).
    pub has_expiry: bool,
    /// A `spec.lifetime` entry ends the whole entity by `rolling` (PL-80).
    pub has_rolling_entity_lifetime: bool,
}

impl SourceDeletes {
    /// The checked number of misses (PL-90, PL-91).
    pub fn misses(&self) -> u8 {
        self.misses.unwrap_or(MISSES_DEFAULT)
    }

    /// The checked share in percent (PL-90, PL-91).
    pub fn max_share(&self) -> u8 {
        self.max_share.unwrap_or(MAX_SHARE_DEFAULT)
    }

    /// Admission of the rule (PL-90, PL-93), each refusal naming its field.
    pub fn validate(&self, context: &Context) -> Result<()> {
        let refused = |field: &str, reason: &str| Error::Invalid {
            field: format!("spec.sourceDeletes.{field}"),
            reason: reason.to_owned(),
        };
        let snapshot = self.mode == SourceDeleteMode::Snapshot;
        let events = self.mode == SourceDeleteMode::Events;
        if context.has_expiry {
            return Err(refused(
                "mode",
                "spec.expiry is the age sweep of a pipeline with no source-delete rule; keep one \
                 of the two (PL-93)",
            ));
        }
        if !snapshot && (self.misses.is_some() || self.max_share.is_some()) {
            let field = if self.misses.is_some() {
                "misses"
            } else {
                "maxShare"
            };
            return Err(refused(
                field,
                "only a snapshot counts misses and shares (PL-90)",
            ));
        }
        if !events && self.deleted_when.is_some() {
            return Err(refused(
                "deletedWhen",
                "only mode events reads a delete marker (PL-90)",
            ));
        }
        match self.mode {
            SourceDeleteMode::Never => {}
            SourceDeleteMode::Snapshot => {
                if !(1..=MISSES_MAX).contains(&self.misses()) {
                    return Err(refused("misses", "misses is 1 to 10 complete runs (PL-90)"));
                }
                if !(1..=100).contains(&self.max_share()) {
                    return Err(refused("maxShare", "maxShare is 1 to 100 percent (PL-90)"));
                }
                if !context.has_clock {
                    return Err(refused(
                        "mode",
                        "a snapshot compares complete runs, and a pipeline with no period or \
                         schedule waits for pushes and has none; use events or lease (PL-93)",
                    ));
                }
            }
            SourceDeleteMode::Events => {
                let marker = self.deleted_when.as_deref().unwrap_or_default();
                if marker.trim().is_empty() {
                    return Err(refused(
                        "deletedWhen",
                        "mode events needs deletedWhen, the query that marks a mapped record as \
                         deleted at the source (PL-90)",
                    ));
                }
            }
            SourceDeleteMode::Lease => {
                if !context.has_rolling_entity_lifetime {
                    return Err(refused(
                        "mode",
                        "mode lease needs a spec.lifetime entry whose entity is rolling (PL-80, \
                         PL-90)",
                    ));
                }
            }
        }
        Ok(())
    }
}
