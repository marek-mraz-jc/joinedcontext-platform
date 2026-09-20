//! `plan` and `apply` over the seed entities of a repository (CC-72, CC-18, T-0421).
//!
//! The repository declares them, the broker holds them, and the difference is the whole live
//! state a reconciler has left to converge: everything else a component reads from Git by
//! itself. A second run over an unchanged repository sends no write, which is what makes
//! `apply` safe to run on every restore, every upgrade and every hour.

use crate::entities::{action, seed_entities, Action, SeedEntity, SeedError};
use crate::gateway::{Broker, BrokerError};
use std::path::Path;

/// One declared entity and what the platform needs done about it.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityChange {
    /// The Context Space it belongs to.
    pub space: String,
    /// Its NGSI-LD id.
    pub id: String,
    /// What `apply` would do, or did.
    pub action: Action,
}

/// What `plan` found, or what `apply` did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Report {
    /// Every declared entity, in repository order.
    pub changes: Vec<EntityChange>,
}

impl Report {
    /// How many entities the platform would have to be told about.
    pub fn pending(&self) -> usize {
        self.changes
            .iter()
            .filter(|change| change.action != Action::Unchanged)
            .count()
    }

    /// Whether the broker already holds every declared entity as declared.
    pub fn is_clean(&self) -> bool {
        self.pending() == 0
    }

    /// The report as JSON, which is what `--json` prints and what a scheduled run parses.
    ///
    /// The same shape the resource report uses (API/03 §6): a summary a dashboard reads
    /// without walking the list, and one object per entity that is not as declared. An
    /// unchanged entity is left out — a report is what to act on, not an inventory.
    pub fn to_json(&self) -> serde_json::Value {
        let drifted: Vec<serde_json::Value> = self
            .changes
            .iter()
            .filter(|change| change.action != Action::Unchanged)
            .map(|change| {
                serde_json::json!({
                    "space": change.space,
                    "id": change.id,
                    "drift": match change.action {
                        Action::Create => "MISSING",
                        _ => "MODIFIED",
                    },
                    // A live entity is data, and data is not adopted into the repository
                    // (CC-38): reverting is the one resolution a seed entity has.
                    "resolutions": ["revert"],
                })
            })
            .collect();
        serde_json::json!({
            "summary": { "checked": self.changes.len(), "drifted": drifted.len() },
            "drifted": drifted,
        })
    }

    /// One line per entity, and a last line that counts them.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for change in &self.changes {
            out.push_str(&format!(
                "{:<9} {} {}\n",
                match change.action {
                    Action::Create => "create",
                    Action::Update => "update",
                    Action::Unchanged => "unchanged",
                },
                change.space,
                change.id
            ));
        }
        out.push_str(&format!(
            "{} seed entities, {} to write\n",
            self.changes.len(),
            self.pending()
        ));
        out
    }
}

/// Why the seed entities could not be planned or applied.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SeedRunError {
    /// The repository's seed entities could not be read.
    #[error(transparent)]
    Repository(#[from] SeedError),
    /// The platform could not be read or refused a write.
    #[error(transparent)]
    Platform(#[from] BrokerError),
}

/// What `apply` would do, without doing any of it (CC-15).
pub fn plan(repo_dir: &Path, broker: &impl Broker) -> Result<Report, SeedRunError> {
    let declared = seed_entities(repo_dir)?;
    Ok(Report {
        changes: compare(&declared, broker)?,
    })
}

/// Replays the seed entities the broker does not already hold as declared (CC-50, CC-72).
///
/// The entities of one space go up in one `entityOperations/upsert`, in repository order, and
/// a space whose entities are all unchanged is not called at all: an unchanged repository
/// issues no writing call, which is what the runbooks rely on when they replay after a
/// restore and then ask for an empty diff.
pub fn apply(repo_dir: &Path, broker: &impl Broker) -> Result<Report, SeedRunError> {
    let declared = seed_entities(repo_dir)?;
    let changes = compare(&declared, broker)?;

    let mut space: Option<&str> = None;
    let mut pending: Vec<serde_json::Value> = Vec::new();
    for (entity, change) in declared.iter().zip(&changes) {
        if space != Some(entity.space.as_str()) {
            write(broker, space, &pending)?;
            pending.clear();
            space = Some(entity.space.as_str());
        }
        if change.action != Action::Unchanged {
            pending.push(entity.body.clone());
        }
    }
    write(broker, space, &pending)?;

    Ok(Report { changes })
}

/// The entities of one space, if there are any to write.
fn write(
    broker: &impl Broker,
    space: Option<&str>,
    entities: &[serde_json::Value],
) -> Result<(), SeedRunError> {
    match space {
        Some(space) if !entities.is_empty() => Ok(broker.upsert(space, entities)?),
        _ => Ok(()),
    }
}

/// What the broker answers for each declared entity, in repository order.
fn compare(
    declared: &[SeedEntity],
    broker: &impl Broker,
) -> Result<Vec<EntityChange>, SeedRunError> {
    let mut changes = Vec::with_capacity(declared.len());
    for entity in declared {
        let live = broker.entity(&entity.space, &entity.id)?;
        changes.push(EntityChange {
            space: entity.space.clone(),
            id: entity.id.clone(),
            action: action(&entity.body, live.as_ref()),
        });
    }
    Ok(changes)
}
