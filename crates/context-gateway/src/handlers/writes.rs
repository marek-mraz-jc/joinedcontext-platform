//! What a write passes before the broker applies it: held whole, judged entity by entity against
//! the Endpoint's guards, and its deletes followed to what they reach (GW11, GW17).

use crate::app::{Gateway, MAX_BODY, WRITE_RETRY_SECONDS};
use crate::auth::subject::Subject;
use crate::pdp::evaluator::{self, Constraints, Verdict};
use crate::pdp::{conditional, write_guard};
use crate::proxy::Broker;
use crate::resolver::Endpoint;
use crate::{middleware::tenancy, operations, query};
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::CONTENT_LENGTH;
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode};
use axum::response::IntoResponse;
use jc_core::kinds::Operation;
use jc_core::ProblemDetails;
use serde_json::Value;
use std::collections::BTreeSet;

/// Ids the existence read of a write asks for at once; a batch naming more is read in chunks.
const TARGETS_PER_READ: usize = 100;

/// One write held against the relationships of the space's model (DM-70, T-2740).
pub(crate) struct HeldWrite<'a> {
    pub(crate) gateway: &'a Gateway,
    pub(crate) subject: &'a Subject,
    pub(crate) endpoint: &'a Endpoint,
    pub(crate) headers: &'a HeaderMap,
    pub(crate) path: &'a str,
    pub(crate) params: &'a [(String, String)],
    pub(crate) operation: Operation,
    pub(crate) rules: &'a crate::relationships::RelationshipRules,
}

impl HeldWrite<'_> {
    /// Refuses a write that breaks a relationship, or divides a batch: the entities that break
    /// one leave `sent` and join the refusals the answer merges (GW18's 207).
    pub(crate) async fn check(
        &self,
        sent: &mut Vec<u8>,
        divided: &mut Option<(Vec<String>, Refused)>,
    ) -> Result<(), Box<Response<Body>>> {
        use crate::relationships::{self, Extent, Shape};
        let refuse = |problem: ProblemDetails| Box::new(problem.into_response());

        let addressed = operations::addressed_entity(self.path).map(query::decode);
        let targeted = operations::targeted_attribute(self.path);
        // Removing a required end outright empties it as surely as a null does.
        if self.operation == Operation::DeleteAttrs {
            if let (Some(id), Some(attribute)) = (addressed.as_deref(), targeted) {
                relationships::deleted_attribute(id, attribute, self.rules)
                    .map_err(|violation| refuse(violation.into()))?;
            }
            return Ok(());
        }
        let extent = match self.operation {
            Operation::CreateEntity | Operation::ReplaceEntity | Operation::CreateBatch => {
                Extent::Whole
            }
            // `options=update` makes an upsert a merge into what is there (CIM 009 5.6.8).
            Operation::UpsertBatch
                if !query::first(self.params, "options").is_some_and(|options| {
                    options.split(',').any(|option| option.trim() == "update")
                }) =>
            {
                Extent::Whole
            }
            Operation::UpsertBatch
            | Operation::UpdateBatch
            | Operation::MergeBatch
            | Operation::MergeEntity
            | Operation::AppendAttrs
            | Operation::UpdateAttrs
            | Operation::ReplaceAttrs
            | Operation::UpdateEntity => Extent::Partial,
            // A temporal write records history, and a delete carries no targets.
            _ => return Ok(()),
        };
        if sent.is_empty() {
            return Ok(());
        }
        let Ok(payload) = serde_json::from_slice::<Value>(sent) else {
            // `judge_write` already refused a body that is not JSON.
            return Ok(());
        };
        let batch = matches!(
            self.operation,
            Operation::CreateBatch
                | Operation::UpsertBatch
                | Operation::UpdateBatch
                | Operation::MergeBatch
        );
        let shape = Shape {
            addressed: addressed.as_deref(),
            targeted,
            extent,
        };
        let (Value::Array(entries), true) = (&payload, batch) else {
            let targets = relationships::targets_of(&payload, shape, self.rules)
                .map_err(|violation| refuse(violation.into()))?;
            let found = self
                .readable(&targets)
                .await
                .map_err(|problem| refuse(*problem))?;
            if let Some(violation) = relationships::first_missing(&targets, &found) {
                return Err(refuse(violation.into()));
            }
            let claims = relationships::claims_of(&payload, shape, self.rules);
            if let Some(violation) = self
                .untaken(&claims, &[])
                .await
                .map_err(|problem| refuse(*problem))?
            {
                return Err(refuse(violation.into()));
            }
            return Ok(());
        };

        // Each entity on its own, then one existence read for the targets of all of them.
        let mut refused: Refused = Vec::new();
        let mut kept: Vec<(usize, relationships::Targets)> = Vec::new();
        for (at, entry) in entries.iter().enumerate() {
            match relationships::targets_of(entry, shape, self.rules) {
                Ok(targets) => kept.push((at, targets)),
                Err(violation) => {
                    refused.push((batch_entry_id(entry).unwrap_or_default(), violation.into()))
                }
            }
        }
        let mut all = relationships::Targets::new();
        for (_, targets) in &kept {
            all.extend(
                targets
                    .iter()
                    .map(|(urn, named)| (urn.clone(), named.clone())),
            );
        }
        let found = self
            .readable(&all)
            .await
            .map_err(|problem| refuse(*problem))?;
        let mut permitted = Vec::new();
        // One-to-one targets in the batch's order: the first entity that names one takes it.
        let mut taken: Vec<relationships::Claim> = Vec::new();
        for (at, targets) in kept {
            let entry = &entries[at];
            if let Some(violation) = relationships::first_missing(&targets, &found) {
                refused.push((batch_entry_id(entry).unwrap_or_default(), violation.into()));
                continue;
            }
            let claims = relationships::claims_of(entry, shape, self.rules);
            match self
                .untaken(&claims, &taken)
                .await
                .map_err(|problem| refuse(*problem))?
            {
                Some(violation) => {
                    refused.push((batch_entry_id(entry).unwrap_or_default(), violation.into()))
                }
                None => {
                    taken.extend(claims);
                    permitted.push(entry.clone());
                }
            }
        }
        if refused.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = permitted.iter().filter_map(batch_entry_id).collect();
        divide(divided, ids, refused, permitted.is_empty())?;
        *sent = serde_json::to_vec(&permitted).map_err(|error| {
            tracing::error!(%error, "the entities a relationship check kept do not serialize");
            refuse(ProblemDetails::internal())
        })?;
        Ok(())
    }

    /// The first one-to-one target of `claims` that another source stores, or that `earlier`
    /// claims of this write already took (DM-70 `target-taken`). A read of the space right
    /// before the write, not a constraint in the store: two writes inside the window between
    /// this read and the broker's write can both take one target (Architecture/11 §1.2, T-2858).
    async fn untaken(
        &self,
        claims: &[crate::relationships::Claim],
        earlier: &[crate::relationships::Claim],
    ) -> Result<Option<crate::relationships::Violation>, Box<ProblemDetails>> {
        let all: Vec<_> = earlier.iter().chain(claims).cloned().collect();
        if let Some(twice) = crate::relationships::claimed_twice(&all) {
            return Ok(Some(twice.taken()));
        }
        let mut asked = BTreeSet::new();
        for claim in claims {
            if !asked.insert((&claim.class, &claim.slot, &claim.object)) {
                continue;
            }
            // Two answers are enough: the holder itself, and anyone else.
            let storing = storing(
                &self.gateway.broker,
                self.headers,
                &claim.class,
                &claim.slot,
                &claim.object,
                2,
                0,
            )
            .await?;
            let other = storing
                .iter()
                .filter_map(|entity| entity.get("id").and_then(Value::as_str))
                .any(|id| Some(id) != claim.holder.as_deref());
            if other {
                return Ok(Some(claim.taken()));
            }
        }
        Ok(None)
    }

    /// The targets the writer can read in the space: one read through this endpoint under the
    /// caller's own read grants, so a target they may not read is as absent as one that is not
    /// there, and a refusal tells them nothing a read would not (DM-71).
    pub(crate) async fn readable(
        &self,
        targets: &crate::relationships::Targets,
    ) -> Result<BTreeSet<String>, Box<ProblemDetails>> {
        let mut found = BTreeSet::new();
        if targets.is_empty() {
            return Ok(found);
        }
        let requested = evaluator::Request {
            types: targets.values().map(|named| named.target.clone()).collect(),
            ..Default::default()
        };
        let Some(read) = read_grant(self.gateway, self.subject, self.endpoint, &requested) else {
            return Ok(found);
        };
        // The grant's filters narrow the read; the attributes and the time window would only
        // hide the entity, which is all this read looks for. One read per grant, so no grant's
        // types pass on another grant's filter (F-PLT2-004, ADR 006).
        let mut probe = (*read).clone();
        probe.attrs.clear();
        probe.temporal_q = None;
        probe.temporal_windows.clear();
        let probes = if probe.grants.is_empty() {
            vec![probe]
        } else {
            crate::pdp::grants::per_grant(&probe)
        };
        let urns: Vec<&String> = targets.keys().collect();
        for probe in &probes {
            let narrowed = query::upstream(&[], probe, &[]);
            for chunk in urns.chunks(TARGETS_PER_READ) {
                let ids = chunk
                    .iter()
                    .map(|urn| query::encode(urn))
                    .collect::<Vec<_>>()
                    .join(",");
                let answer = conditional::retrieve(
                    &self.gateway.broker,
                    // Only which ids exist is asked (`pick=id`, CIM 009 4.21): a target with a
                    // polygon or a long history is megabytes, and a chunk of them outgrew the
                    // probe's cap and answered every write 500 (praha CityDistrict, T-2971).
                    &format!(
                        "/ngsi-ld/v1/entities?id={ids}&pick=id&limit={}&{narrowed}",
                        chunk.len()
                    ),
                    self.headers,
                )
                .await?;
                if !(200..300).contains(&answer.status) {
                    tracing::error!(
                        status = answer.status,
                        "the broker refused the read of a write's relationship targets"
                    );
                    return Err(Box::new(ProblemDetails::new(
                        502,
                        "upstream-unavailable",
                        "Broker Unavailable",
                    )));
                }
                found.extend(
                    answer
                        .body
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|entity| entity.get("id").and_then(Value::as_str))
                        .filter(|id| write_guard::check_granted_id(id, probe).is_ok())
                        .map(str::to_owned),
                );
            }
        }
        Ok(found)
    }
}

/// The caller's read grant on `endpoint` over `requested`: a query by `GET /entities` or by
/// `entityOperations/query`, which read the same entities. A Pipeline's write Policy grants only
/// the second (upsertBatch, createBatch, queryBatch), and without it every link a pipeline wrote
/// was refused as a missing target (T-5720).
fn read_grant(
    gateway: &Gateway,
    subject: &Subject,
    endpoint: &Endpoint,
    requested: &evaluator::Request,
) -> Option<Box<Constraints>> {
    [Operation::QueryEntity, Operation::QueryBatch]
        .into_iter()
        .find_map(
            |operation| match gateway.pdp.decide(subject, operation, requested, endpoint) {
                Verdict::Rewrite(read) if !read.empty => Some(read),
                _ => None,
            },
        )
}

/// The `q` that finds the entities whose `slot` names `object`. The object comes from the
/// writer's payload, and a quote in it would close the string and add terms of its own, so an
/// object holding `"` or `\`, which no URI does, is refused (F-PLT2-010).
fn object_query(slot: &str, object: &str) -> Result<String, Box<ProblemDetails>> {
    if object.is_empty() || object.contains(['"', '\\']) {
        return Err(Box::new(
            ProblemDetails::bad_request()
                .with_field(slot)
                .with_detail(format!(
                    "the relationship `{slot}` names an object that is no URI: it holds a quote or a backslash"
                )),
        ));
    }
    Ok(format!("{slot}==\"{object}\""))
}

/// The entities of `class` whose stored end `slot` names `object`: the computed end's query
/// (DM-67). Read with the pinned tenant and none of the caller's grants, because a relationship
/// rule holds for the whole space; what the caller may see of the answer is decided by whoever
/// uses it.
pub(crate) async fn storing(
    broker: &Broker,
    headers: &HeaderMap,
    class: &str,
    slot: &str,
    object: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<Value>, Box<ProblemDetails>> {
    let q = query::encode(&object_query(slot, object)?);
    let target = format!(
        "/ngsi-ld/v1/entities?type={}&q={q}&attrs={}&limit={limit}&offset={offset}",
        query::encode(class),
        query::encode(slot),
    );
    let answer = conditional::retrieve(broker, &target, headers).await?;
    if !(200..300).contains(&answer.status) {
        tracing::error!(
            status = answer.status,
            "the broker refused the read of the entities that point at a target"
        );
        return Err(Box::new(ProblemDetails::new(
            502,
            "upstream-unavailable",
            "Broker Unavailable",
        )));
    }
    Ok(match answer.body {
        Value::Array(entities) => entities,
        _ => Vec::new(),
    })
}

/// Entities a delete's rules may reach, counting the ones they change and the ones they delete
/// with it; a delete that reaches more is refused before anything changes (DM-71).
const REACHED_PER_DELETE: usize = 500;

/// Entities that point at a target, read per page.
const POINTING_PER_READ: usize = 100;

/// One change a delete's rules make to an entity that points at what it deletes (DM-66).
#[derive(Debug)]
pub(crate) enum Unlinking {
    /// The entity goes with its target (`cascade` on a single end).
    Delete(String),
    /// The whole attribute goes (`set-null` on a single end).
    DeleteAttribute { id: String, slot: String },
    /// One link instance of a many end goes.
    DeleteInstance {
        id: String,
        slot: String,
        dataset: Option<String>,
    },
    /// A link instance whose `object` list named the target keeps its other targets.
    Rewrite {
        id: String,
        slot: String,
        dataset: Option<String>,
        objects: Vec<String>,
    },
}

/// One delete held against the relationships of the space's model (DM-71, T-2858).
///
/// The broker keeps no relationship rules (the owner's decision of 2026-09-25), so the rules run
/// here as the gateway's own writes: every entity that points at what is deleted is read, the
/// delete is refused while one of them keeps it (`restrict`, or a rule that would empty a
/// required end, or an entity the caller may not change), and otherwise the entities are changed
/// or deleted first and the entity itself last. Nothing is changed until every rule has passed;
/// a reference written after the read is the race window Architecture/11 §1.2 names.
/// The most ids one batch delete names: the broker's own batch ceiling (Antares
/// `ANTARES_MAX_BATCH_ITEMS`, 1000 unless raised).
const MAX_DELETE_IDS: usize = 1_000;

pub(crate) struct HeldDelete<'a> {
    pub(crate) gateway: &'a Gateway,
    pub(crate) subject: &'a Subject,
    pub(crate) endpoint: &'a Endpoint,
    pub(crate) headers: &'a HeaderMap,
    pub(crate) rules: &'a crate::relationships::RelationshipRules,
}

impl HeldDelete<'_> {
    /// Runs the rules of a delete of one entity; the entity itself is the forwarded delete's.
    pub(crate) async fn entity(&self, id: &str) -> Result<(), Box<Response<Body>>> {
        let answer = |problem: Box<ProblemDetails>| Box::new(problem.into_response());
        let steps = self.plan(id).await.map_err(answer)?;
        self.apply(id, &steps).await.map_err(answer)
    }

    /// Runs the rules of a batch delete per id: an id the rules keep is refused in the batch's
    /// answer (207), the others go on. Answers whether `sent` was rewritten.
    pub(crate) async fn batch(
        &self,
        sent: &mut Vec<u8>,
        divided: &mut Option<(Vec<String>, Refused)>,
    ) -> Result<bool, Box<Response<Body>>> {
        let answer = |problem: Box<ProblemDetails>| Box::new(problem.into_response());
        let Ok(Value::Array(entries)) = serde_json::from_slice::<Value>(sent) else {
            return Ok(false);
        };
        // Each id costs a relationship plan of broker reads, so the ids are counted before any
        // is planned (F-SEC1-016); the broker refuses a larger batch anyway.
        if entries.len() > MAX_DELETE_IDS {
            return Err(answer(Box::new(ProblemDetails::bad_request().with_detail(
                format!("a batch delete takes at most {MAX_DELETE_IDS} ids; send the rest in another batch"),
            ))));
        }
        let mut refused: Refused = Vec::new();
        let mut permitted = Vec::new();
        let mut plans = Vec::new();
        for entry in entries {
            let Some(id) = entry.as_str() else {
                permitted.push(entry);
                continue;
            };
            match self.plan(id).await {
                Ok(steps) => {
                    plans.push((id.to_owned(), steps));
                    permitted.push(entry);
                }
                Err(problem) if problem.status == 409 => refused.push((id.to_owned(), *problem)),
                Err(problem) => return Err(answer(problem)),
            }
        }
        for (id, steps) in &plans {
            self.apply(id, steps).await.map_err(answer)?;
        }
        if refused.is_empty() {
            return Ok(false);
        }
        let ids: Vec<String> = permitted.iter().filter_map(batch_entry_id).collect();
        divide(divided, ids, refused, permitted.is_empty())?;
        *sent = serde_json::to_vec(&permitted).map_err(|error| {
            tracing::error!(%error, "the ids a delete's rules kept do not serialize");
            answer(Box::new(ProblemDetails::internal()))
        })?;
        Ok(true)
    }

    /// What the rules of a delete of `root` change, in the order they are applied: the
    /// attributes and links first, then the entities deleted with it, farthest first, so a stop
    /// part way never leaves a reference to something already gone. Reads only.
    pub(crate) async fn plan(&self, root: &str) -> Result<Vec<Unlinking>, Box<ProblemDetails>> {
        use crate::relationships::{unlink, Unlink};
        let mut edits = Vec::new();
        let mut deleted = Vec::new();
        let mut going = BTreeSet::from([root.to_owned()]);
        let mut queue = std::collections::VecDeque::from([root.to_owned()]);
        while let Some(id) = queue.pop_front() {
            for at in self.rules.referencing_id(&id) {
                let pointing = self.pointing(at.class, at.slot, &id).await?;
                for entity in &pointing {
                    let Some(other) = entity.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    // An entity deleted by this same delete keeps nothing and needs no change.
                    if going.contains(other) {
                        continue;
                    }
                    match unlink(entity, at.slot, at.end, &id) {
                        Unlink::Refused => {
                            return Err(self.kept(at.class, at.slot, &id, &pointing, true));
                        }
                        Unlink::DeleteEntity => {
                            self.may(
                                Operation::DeleteEntity,
                                at.class,
                                other,
                                at.slot,
                                &id,
                                &pointing,
                            )?;
                            going.insert(other.to_owned());
                            queue.push_back(other.to_owned());
                            deleted.push(Unlinking::Delete(other.to_owned()));
                        }
                        Unlink::DeleteAttribute => {
                            self.may(
                                Operation::DeleteAttrs,
                                at.class,
                                other,
                                at.slot,
                                &id,
                                &pointing,
                            )?;
                            edits.push(Unlinking::DeleteAttribute {
                                id: other.to_owned(),
                                slot: at.slot.to_owned(),
                            });
                        }
                        Unlink::Instances { removed, rewritten } => {
                            if !removed.is_empty() {
                                self.may(
                                    Operation::DeleteAttrs,
                                    at.class,
                                    other,
                                    at.slot,
                                    &id,
                                    &pointing,
                                )?;
                            }
                            if !rewritten.is_empty() {
                                self.may(
                                    Operation::UpdateAttrs,
                                    at.class,
                                    other,
                                    at.slot,
                                    &id,
                                    &pointing,
                                )?;
                            }
                            edits.extend(removed.into_iter().map(|dataset| {
                                Unlinking::DeleteInstance {
                                    id: other.to_owned(),
                                    slot: at.slot.to_owned(),
                                    dataset,
                                }
                            }));
                            edits.extend(rewritten.into_iter().map(|(dataset, objects)| {
                                Unlinking::Rewrite {
                                    id: other.to_owned(),
                                    slot: at.slot.to_owned(),
                                    dataset,
                                    objects,
                                }
                            }));
                        }
                    }
                    if going.len() + edits.len() > REACHED_PER_DELETE {
                        return Err(Box::new(too_far(root)));
                    }
                }
            }
        }
        edits.extend(deleted.into_iter().rev());
        Ok(edits)
    }

    /// Every entity of `class` whose end `slot` points at `id`, page by page.
    pub(crate) async fn pointing(
        &self,
        class: &str,
        slot: &str,
        id: &str,
    ) -> Result<Vec<Value>, Box<ProblemDetails>> {
        let mut all = Vec::new();
        loop {
            let page = storing(
                &self.gateway.broker,
                self.headers,
                class,
                slot,
                id,
                POINTING_PER_READ,
                all.len(),
            )
            .await?;
            let last = page.len() < POINTING_PER_READ;
            all.extend(page);
            if last {
                return Ok(all);
            }
            if all.len() > REACHED_PER_DELETE {
                return Err(Box::new(too_far(id)));
            }
        }
    }

    /// Refuses the change unless the caller's own grants allow `operation` on `other`: a rule
    /// never reaches an entity its caller could not have changed by hand. A grant that decides
    /// from the stored entity is not evaluated here and is held as a refusal.
    pub(crate) fn may(
        &self,
        operation: Operation,
        class: &str,
        other: &str,
        slot: &str,
        id: &str,
        pointing: &[Value],
    ) -> Result<(), Box<ProblemDetails>> {
        let requested = evaluator::Request {
            types: BTreeSet::from([class.to_owned()]),
            ..Default::default()
        };
        let allowed =
            match self
                .gateway
                .pdp
                .decide(self.subject, operation, &requested, self.endpoint)
            {
                Verdict::Rewrite(granted) => {
                    !granted.empty
                        && !granted.hidden.contains(slot)
                        && !conditional::state_dependent(&granted)
                        && write_guard::check_granted_id(other, &granted).is_ok()
                }
                _ => false,
            };
        if allowed {
            Ok(())
        } else {
            Err(self.kept(class, slot, id, pointing, false))
        }
    }

    /// 409 for a delete the rules keep (DM-71): the slot and how many point at the target,
    /// their ids only as far as the caller may read them, and never an id when the reason is an
    /// entity the caller may not change.
    pub(crate) fn kept(
        &self,
        class: &str,
        slot: &str,
        id: &str,
        pointing: &[Value],
        by_rule: bool,
    ) -> Box<ProblemDetails> {
        let count = pointing.len();
        let detail = if by_rule {
            format!(
                "{count} `{class}` still point at `{id}` through `{slot}`, and the model's delete \
                 rule keeps it while they do; unlink or delete them first (DM-71)"
            )
        } else {
            format!(
                "{count} `{class}` point at `{id}` through `{slot}`, and the delete rule would \
                 change one you may not change; ask someone who may, or unlink it first (DM-71)"
            )
        };
        let ids: Vec<Value> = if by_rule {
            self.readable_ids(class, pointing)
        } else {
            Vec::new()
        };
        Box::new(
            ProblemDetails::conflict()
                .with_detail(detail)
                .with_extension("rule", Value::String("restrict".to_owned()))
                .with_extension("slot", Value::String(slot.to_owned()))
                .with_extension("count", Value::from(count))
                .with_extension("ids", Value::Array(ids)),
        )
    }

    /// The ids of `pointing` the caller's read grants on this endpoint select.
    fn readable_ids(&self, class: &str, pointing: &[Value]) -> Vec<Value> {
        let requested = evaluator::Request {
            types: BTreeSet::from([class.to_owned()]),
            ..Default::default()
        };
        let Some(read) = read_grant(self.gateway, self.subject, self.endpoint, &requested) else {
            return Vec::new();
        };
        if conditional::state_dependent(&read) {
            return Vec::new();
        }
        pointing
            .iter()
            .filter_map(|entity| entity.get("id").and_then(Value::as_str))
            .filter(|id| write_guard::check_granted_id(id, &read).is_ok())
            .map(|id| Value::String(id.to_owned()))
            .collect()
    }

    /// Applies the planned changes in order. A step the broker refuses stops the delete with the
    /// entity still in place, and the answer says how many changes were already made.
    pub(crate) async fn apply(
        &self,
        root: &str,
        steps: &[Unlinking],
    ) -> Result<(), Box<ProblemDetails>> {
        for (done, step) in steps.iter().enumerate() {
            let entity = |id: &str| format!("/ngsi-ld/v1/entities/{}", query::encode(id));
            let attribute =
                |id: &str, slot: &str| format!("{}/attrs/{}", entity(id), query::encode(slot));
            let (method, target, body) = match step {
                Unlinking::Delete(id) => (Method::DELETE, entity(id), None),
                Unlinking::DeleteAttribute { id, slot } => {
                    (Method::DELETE, attribute(id, slot), None)
                }
                Unlinking::DeleteInstance { id, slot, dataset } => {
                    let target = match dataset {
                        Some(dataset) => format!(
                            "{}?datasetId={}",
                            attribute(id, slot),
                            query::encode(dataset)
                        ),
                        None => attribute(id, slot),
                    };
                    (Method::DELETE, target, None)
                }
                Unlinking::Rewrite {
                    id,
                    slot,
                    dataset,
                    objects,
                } => {
                    let mut value =
                        serde_json::json!({ "type": "Relationship", "object": objects });
                    if let Some(dataset) = dataset {
                        value["datasetId"] = Value::String(dataset.clone());
                    }
                    (Method::PATCH, attribute(id, slot), Some(value.to_string()))
                }
            };
            let mut headers = HeaderMap::new();
            if let Some(tenant) = self.headers.get(tenancy::TENANT) {
                headers.insert(tenancy::TENANT, tenant.clone());
            }
            tenancy::as_gateway(&mut headers);
            if body.is_some() {
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
            }
            let body = body.map_or_else(Body::empty, Body::from);
            let status = match self
                .gateway
                .broker
                .send(method, &target, headers, body)
                .await
            {
                Ok(answer) => answer.status(),
                Err(error) => {
                    tracing::error!(%error, "the broker did not take a delete rule's change");
                    StatusCode::BAD_GATEWAY
                }
            };
            // An entity another delete already took is gone either way; a link or an attribute
            // that is not found was not removed by this delete, so it stops like any refusal.
            let gone = status == StatusCode::NOT_FOUND && matches!(step, Unlinking::Delete(_));
            if !status.is_success() && !gone {
                tracing::error!(%status, %target, "the broker refused a delete rule's change");
                return Err(Box::new(
                    ProblemDetails::new(502, "upstream-unavailable", "Broker Unavailable")
                        .with_detail(format!(
                            "`{root}` is not deleted: the broker refused change {} of {} its \
                             delete rules make, after {done} were made (DM-71)",
                            done + 1,
                            steps.len()
                        ))
                        .with_extension("changed", Value::from(done)),
                ));
            }
        }
        Ok(())
    }
}

/// 409 for a delete whose rules reach more entities than one delete may change.
fn too_far(id: &str) -> ProblemDetails {
    ProblemDetails::conflict()
        .with_detail(format!(
            "deleting `{id}` would reach more than {REACHED_PER_DELETE} entities through the \
             model's delete rules; delete them in smaller parts first (DM-71)"
        ))
        .with_extension("rule", Value::String("restrict".to_owned()))
}

/// Checks and fills the units of a write body in place (DM-06), answering the quantities it
/// filled. The body is rewritten only when something was filled.
pub(crate) fn filled_units(
    sent: &mut Vec<u8>,
    path: &str,
    rules: &crate::units::UnitRules,
) -> Result<Vec<String>, Box<ProblemDetails>> {
    if rules.is_empty() {
        return Ok(Vec::new());
    }
    let Ok(mut payload) = serde_json::from_slice::<Value>(sent) else {
        // `judge_write` already refused a body that is not JSON; nothing is left to check.
        return Ok(Vec::new());
    };
    let addressed = operations::addressed_entity(path).map(query::decode);
    let shape = crate::units::Shape {
        addressed: addressed.as_deref(),
        targeted: operations::targeted_attribute(path),
    };
    let filled = crate::units::enforce(&mut payload, shape, rules)
        .map_err(|refusal| Box::new(ProblemDetails::from(refusal)))?;
    if !filled.is_empty() {
        *sent = serde_json::to_vec(&payload).map_err(|error| {
            tracing::error!(%error, "a write with its units filled does not serialize");
            Box::new(ProblemDetails::internal())
        })?;
    }
    Ok(filled)
}

/// DM-121: the write as the broker may keep it, every personal slot the model drops or hashes
/// (ADR-N-054 2.6) minimised in `sent`. A refusal names the attribute, never its value.
pub(crate) async fn minimised(
    gateway: &Gateway,
    endpoint: &Endpoint,
    path: &str,
    schema: &jc_manifest::model_schema::ModelSchema,
    sent: &mut Vec<u8>,
) -> Result<(), Box<ProblemDetails>> {
    use crate::ingest::{self, Refusal};
    let key = if ingest::wants_key(schema) {
        ingest::key_of(gateway.personal_keys.as_deref(), &endpoint.space)
    } else {
        None
    };
    let Ok(mut payload) = serde_json::from_slice::<Value>(sent) else {
        return Err(Box::new(
            ProblemDetails::bad_request().with_detail("the body is not JSON"),
        ));
    };
    let addressed = operations::addressed_entity(path).map(query::decode);
    let shape = crate::units::Shape {
        addressed: addressed.as_deref(),
        targeted: operations::targeted_attribute(path),
    };
    match ingest::apply(&mut payload, shape, schema, key.as_ref()) {
        Ok(false) => Ok(()),
        Ok(true) => {
            *sent = serde_json::to_vec(&payload).map_err(|error| {
                tracing::error!(%error, "a minimised write does not serialize");
                Box::new(ProblemDetails::internal())
            })?;
            Ok(())
        }
        Err(Refusal::NoKey { attribute }) => {
            tracing::error!(slug = %endpoint.slug, space = %endpoint.space, %attribute,
                "write refused: the space's personal-data key is not mounted");
            Err(Box::new(
                ProblemDetails::new(503, "service-unavailable", "Service Unavailable").with_detail(
                    format!(
                    "`{attribute}` is stored as a keyed hash and the space's key is not available"
                ),
                ),
            ))
        }
        Err(Refusal::Unknown { attribute, mode }) => {
            tracing::error!(slug = %endpoint.slug, %attribute, %mode,
                "write refused: the model names an ingest the gateway does not know");
            Err(Box::new(ProblemDetails::internal()))
        }
        Err(Refusal::Unfit { attribute }) => {
            tracing::info!(slug = %endpoint.slug, %attribute, "write refused: a personal slot it cannot minimise");
            Err(Box::new(ProblemDetails::bad_request().with_detail(format!(
                "the model stores `{attribute}` as a keyed hash of a Property's value, or not at all"
            ))))
        }
    }
}

/// The quarantine mark is written by the space's rules alone (DM-90): a write whose path
/// addresses it is refused, a `DELETE` included, and a body's own copy is dropped. `true` when
/// the body changed.
pub(crate) fn owned_guarded(sent: &mut Vec<u8>, path: &str) -> Result<bool, Box<ProblemDetails>> {
    let targeted = operations::targeted_attribute(path);
    if let Some(problem) = crate::quality::owned_path(targeted) {
        tracing::info!("write refused: it addresses the quarantine mark");
        return Err(Box::new(problem));
    }
    if sent.is_empty() || targeted.is_some() {
        return Ok(false);
    }
    let Ok(mut payload) = serde_json::from_slice::<Value>(sent) else {
        return Ok(false);
    };
    if !crate::quality::strip_owned(&mut payload) {
        return Ok(false);
    }
    *sent = serde_json::to_vec(&payload).map_err(|error| {
        tracing::error!(%error, "a write without the quarantine mark does not serialize");
        Box::new(ProblemDetails::internal())
    })?;
    Ok(true)
}

/// Judges a write against the slot constraints of the space's model (DM-86) and the space's
/// quality rules (DM-87..DM-93): a broken constraint or a rejecting rule refuses it, a flag or
/// a quarantine is written into `sent` before it is forwarded.
///
/// A batch is judged per entity, as GW18 and the relationships do: the members
/// refused leave `sent` and join `divided`, so one bad record of a pipeline's page of 1000 no
/// longer costs the other 999 (T-4011); a batch refused whole is a 207 naming every member.
pub(crate) fn quality_judged(
    sent: &mut Vec<u8>,
    path: &str,
    operation: Operation,
    endpoint: &Endpoint,
    divided: &mut Option<(Vec<String>, Refused)>,
) -> Result<(), Box<Response<Body>>> {
    let Some(declared) = endpoint.declared_types.as_ref() else {
        return Ok(());
    };
    if declared.schema.is_none() && declared.quality.is_empty() {
        return Ok(());
    }
    let Ok(mut payload) = serde_json::from_slice::<Value>(sent) else {
        // `judge_write` already refused a body that is not JSON; nothing is left to check.
        return Ok(());
    };
    let addressed = operations::addressed_entity(path).map(query::decode);
    let shape = crate::units::Shape {
        addressed: addressed.as_deref(),
        targeted: operations::targeted_attribute(path),
    };
    // A create or a replace carries the whole entity; every other write a part of it.
    let whole = matches!(
        operation,
        Operation::CreateEntity | Operation::CreateBatch | Operation::ReplaceEntity
    );
    let judge = |payload: &mut Value| -> Result<(), Box<ProblemDetails>> {
        if let Some(schema) = &declared.schema {
            crate::quality::check_model(payload, shape, whole, schema, &endpoint.space)?;
        }
        crate::quality::enforce(
            payload,
            shape,
            &declared.quality,
            &endpoint.space,
            chrono::Utc::now(),
        )
    };
    let refuse = |problem: Box<ProblemDetails>| Box::new(problem.into_response());
    // Only a rule writes into the payload (a flag, a quarantine); the model check reads it. A
    // space without rules keeps no second copy of a page of 1000 entities (T-4077).
    let before = (!declared.quality.is_empty()).then(|| payload.clone());
    let mut changed = false;
    let batch = matches!(
        operation,
        Operation::CreateBatch
            | Operation::UpsertBatch
            | Operation::UpdateBatch
            | Operation::MergeBatch
    );
    match &mut payload {
        Value::Array(entries) if batch => {
            let mut refused: Refused = Vec::new();
            entries.retain_mut(|entry| match judge(entry) {
                Ok(()) => true,
                Err(problem) => {
                    refused.push((batch_entry_id(entry).unwrap_or_default(), *problem));
                    false
                }
            });
            if !refused.is_empty() {
                changed = true;
                let ids: Vec<String> = entries.iter().filter_map(batch_entry_id).collect();
                divide(divided, ids, refused, entries.is_empty())?;
            }
        }
        payload => judge(payload).map_err(refuse)?,
    }
    if changed || before.is_some_and(|before| before != payload) {
        *sent = serde_json::to_vec(&payload).map_err(|error| {
            tracing::error!(%error, "a judged write does not serialize");
            refuse(Box::new(ProblemDetails::internal()))
        })?;
    }
    Ok(())
}

/// The answer to a write whose units the gateway filled, saying which (DM-06). A refused
/// write changed nothing, so it says nothing either.
pub(crate) fn with_filled_units(mut answer: Response<Body>, filled: &[String]) -> Response<Body> {
    if filled.is_empty() || !answer.status().is_success() {
        return answer;
    }
    match HeaderValue::from_str(&filled.join(", ")) {
        Ok(value) => {
            answer
                .headers_mut()
                .insert(crate::units::FILLED_HEADER, value);
        }
        Err(_) => {
            tracing::warn!("the filled units do not fit a header; the answer does not name them")
        }
    }
    answer
}

/// The request payload media types the NGSI-LD surface accepts (CIM 009 clause 6.3.5).
const PAYLOAD_TYPES: &[&str] = &[
    "application/json",
    "application/ld+json",
    "application/merge-patch+json",
];

/// Whether the request names a payload media type the surface accepts, parameters aside.
pub(crate) fn accepted_payload(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().trim())
        .is_some_and(|media| {
            PAYLOAD_TYPES
                .iter()
                .any(|accepted| media.eq_ignore_ascii_case(accepted))
        })
}

/// 415, the answer to a payload in a media type the surface does not take (clause 6.3.2).
pub(crate) fn unsupported_media_type() -> ProblemDetails {
    ProblemDetails::new(415, "unsupported-media-type", "Unsupported Media Type").with_detail(
        "the request payload must be application/json, application/ld+json or application/merge-patch+json",
    )
}

/// The size of a request's body as its head states it. A body without Content-Length still knows
/// its bound when it has one: a DELETE's is 0, so it takes the smallest share, not the largest
/// (T-4077 re-open).
pub(crate) fn body_length(request: &Request<Body>) -> Option<usize> {
    request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .or_else(|| {
            axum::body::HttpBody::size_hint(request.body())
                .upper()
                .and_then(|upper| usize::try_from(upper).ok())
        })
}

/// A write's share of the gateway's write budget (T-4077), held from before its body is read
/// until it is answered, so a slow broker fills the budget and not the gateway's memory; a read
/// takes none. A write that waited its whole wait in vain, the 503 to answer
/// instead (T-4123).
pub(crate) async fn write_share(
    gateway: &Gateway,
    endpoint: &Endpoint,
    operation: Operation,
    length: Option<usize>,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, Box<Response<Body>>> {
    if !operation.is_write() {
        return Ok(None);
    }
    match gateway.write_share(length).await {
        Some(share) => Ok(Some(share)),
        None => {
            tracing::info!(slug = %endpoint.slug, %operation, "write answered 503: the gateway held its whole write budget for the write's wait");
            Err(Box::new(busy()))
        }
    }
}

/// The 503 of a write over the gateway's write budget (T-4077): nothing of it was read, so
/// sending it again after `Retry-After` is safe.
pub(crate) fn busy() -> Response<Body> {
    let mut answer = ProblemDetails::new(503, "service-unavailable", "Service Unavailable")
        .with_detail(
            "the gateway holds as many writes as it may at once; send this one again after the \
             seconds Retry-After names",
        )
        .into_response();
    answer
        .headers_mut()
        .insert("retry-after", HeaderValue::from(WRITE_RETRY_SECONDS));
    answer
}

/// GW1 and R20: a refused read of one entity is indistinguishable from a miss; everything
/// else is refused openly, without naming the rule that refused it.
pub(crate) fn refused(operation: Operation, path: &str) -> Response<Body> {
    if !operation.is_write() && operations::addressed_entity(path).is_some() {
        ProblemDetails::not_found().into_response()
    } else {
        ProblemDetails::forbidden().into_response()
    }
}

/// The entities of a batch the gateway refused, each with the id it named and the problem.
pub(crate) type Refused = Vec<(String, ProblemDetails)>;

/// What the write guard makes of a write payload (GW17, GW18).
pub(crate) enum Judged {
    /// Every entity it names is permitted: the body goes on as it was sent.
    Whole,
    /// A batch the grants divide: `permitted` goes on, `refused` is answered per entity id.
    Divided {
        permitted: Vec<Value>,
        refused: Refused,
    },
}

/// The batch operations whose entities are judged one by one (GW18). A batch query is a read
/// and is narrowed instead.
fn divides(operation: Operation) -> bool {
    matches!(
        operation,
        Operation::CreateBatch
            | Operation::UpsertBatch
            | Operation::UpdateBatch
            | Operation::MergeBatch
            | Operation::DeleteBatch
    )
}

/// The entity id a batch entry names: the URN itself in a batch delete, the `id` of an entity
/// otherwise.
pub(crate) fn batch_entry_id(entry: &Value) -> Option<String> {
    entry
        .as_str()
        .or_else(|| {
            entry
                .get("id")
                .or_else(|| entry.get("@id"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

/// Checks a write payload and answers with the problem that refuses it whole, or with what
/// goes on (GW17, GW18).
///
/// The body of a `PATCH .../attrs/{name}` is the bare attribute value, so it is wrapped
/// back under its name before the grant is consulted; anything else is an entity, or a
/// batch of them. An entity is judged whole (GW17); a batch is divided per entity, so the
/// entities the grants permit are still applied (GW18), unless one of them is malformed.
pub(crate) fn judge_write(
    body: &[u8],
    path: &str,
    operation: Operation,
    constraints: &Constraints,
    endpoint: &Endpoint,
) -> Result<Judged, Box<ProblemDetails>> {
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return Err(Box::new(
            ProblemDetails::bad_request().with_detail("request body is not JSON"),
        ));
    };
    let payload = match operations::targeted_attribute(path) {
        Some(name) => serde_json::json!({ name: payload }),
        None => payload,
    };
    // An addressed write changes the entity in its path and nothing else (GW17, PF-44).
    let addressed = operations::addressed_entity(path).map(query::decode);

    let entries = match payload {
        Value::Array(entries) if divides(operation) => entries,
        Value::Array(entities) => {
            for entity in &entities {
                refuse_entity(entity, addressed.as_deref(), constraints, endpoint)?;
            }
            return Ok(Judged::Whole);
        }
        other => {
            refuse_entity(&other, addressed.as_deref(), constraints, endpoint)?;
            return Ok(Judged::Whole);
        }
    };
    // Every refusal of a batch is answered under the id it names, so an entry that names none
    // leaves nothing to answer it under: the request is malformed as a whole.
    if entries.iter().any(|entry| batch_entry_id(entry).is_none()) {
        return Err(Box::new(ProblemDetails::bad_request().with_detail(
            "every entry of a batch operation names its entity by a string id",
        )));
    }
    let mut permitted = Vec::with_capacity(entries.len());
    let mut refused = Vec::new();
    for entry in entries {
        match refuse_entity(&entry, None, constraints, endpoint) {
            Ok(()) => permitted.push(entry),
            // A malformed id is no grant decision but a malformed write, and a batch
            // half applied around it is the one outcome nobody asked for (PF-42, T-1697).
            Err(problem) if problem.status == StatusCode::BAD_REQUEST.as_u16() => {
                return Err(problem);
            }
            Err(problem) => {
                refused.push((batch_entry_id(&entry).unwrap_or_default(), *problem));
            }
        }
    }
    Ok(if refused.is_empty() {
        Judged::Whole
    } else {
        Judged::Divided { permitted, refused }
    })
}

/// Checks one entity of a write, or the fragment of an addressed one, against the grants and
/// the endpoint, and answers with the problem that refuses it (GW17).
fn refuse_entity(
    entity: &Value,
    addressed: Option<&str>,
    constraints: &Constraints,
    endpoint: &Endpoint,
) -> Result<(), Box<ProblemDetails>> {
    if let Some(problem) = undeclared_type(entity, endpoint) {
        return Err(Box::new(problem));
    }
    if let (Some(path_id), Some(object)) = (addressed, entity.as_object()) {
        let named = object.get("id").or_else(|| object.get("@id"));
        if named.is_some_and(|id| id.as_str() != Some(path_id)) {
            return Err(Box::new(
                ProblemDetails::bad_request()
                    .with_detail("the body names another entity than the path"),
            ));
        }
        if let Some(kind) = object.get("type").or_else(|| object.get("@type")) {
            write_guard::check_identifier(path_id, Some(kind.as_str().unwrap_or_default()))
                .map_err(|refusal| Box::new(ProblemDetails::from(refusal)))?;
        }
    }
    // What an Endpoint does not show cannot be changed through it (EP-61, GW17).
    if let Some(hidden) = entity.as_object().and_then(|object| {
        object.keys().find(|key| {
            endpoint
                .hidden_attributes
                .iter()
                .any(|hidden| write_guard::names_attribute(key, hidden))
        })
    }) {
        let refusal = write_guard::Refusal::AttributeOutsideGrant(hidden.clone());
        tracing::info!(slug = %endpoint.slug, %refusal, "write refused");
        return Err(Box::new(ProblemDetails::from(refusal)));
    }
    // A batch delete is an array of URN strings: each is an identifier and nothing else
    // (T-0806).
    let outcome = if let Some(raw) = entity.as_str() {
        write_guard::check_identifier(raw, None)
            .and_then(|()| write_guard::check_granted_id(raw, constraints))
    } else if entity.get("id").is_some() || entity.get("@id").is_some() {
        write_guard::check(entity, constraints)
    } else {
        write_guard::check_fragment(entity, constraints)
    };
    outcome.map_err(|refusal| {
        tracing::info!(slug = %endpoint.slug, %refusal, "write refused");
        Box::new(ProblemDetails::from(refusal))
    })
}

/// The `errors` members of a `BatchOperationResult` for the entities the gateway refused.
/// Merges `refused` into the refusals an earlier check of this write made and keeps `ids` as the
/// entities still sent; a write with nothing left to send is answered by its 207 at once.
fn divide(
    divided: &mut Option<(Vec<String>, Refused)>,
    ids: Vec<String>,
    refused: Refused,
    nothing_left: bool,
) -> Result<(), Box<Response<Body>>> {
    let refused = match divided.take() {
        Some((_, mut earlier)) => {
            earlier.extend(refused);
            earlier
        }
        None => refused,
    };
    if nothing_left {
        return Err(Box::new(batch_result(Vec::new(), refusal_entries(refused))));
    }
    *divided = Some((ids, refused));
    Ok(())
}

pub(crate) fn refusal_entries(refused: Refused) -> Vec<Value> {
    refused
        .into_iter()
        .map(|(id, problem)| serde_json::json!({ "entityId": id, "error": problem }))
        .collect()
}

/// A `207 Multi-Status` carrying a `BatchOperationResult` (CIM 009 clause 5.2.16, GW18).
pub(crate) fn batch_result(success: Vec<Value>, errors: Vec<Value>) -> Response<Body> {
    let body = serde_json::json!({ "success": success, "errors": errors });
    let mut answer = (StatusCode::MULTI_STATUS, body.to_string()).into_response();
    answer.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    answer
}

/// The broker's answer to the permitted part of a divided batch, with the gateway's refusals
/// beside it, as one `BatchOperationResult` (GW18).
///
/// A `207` from the broker is extended; a success without an id list means every forwarded id
/// was applied; a refusal of the whole forwarded part is answered for each of its entities, in
/// the words the gateway already chose for it (a broker failure is never passed on verbatim).
pub(crate) async fn merged_batch(
    answer: Response<Body>,
    forwarded: Vec<String>,
    refused: Refused,
) -> Response<Body> {
    let (parts, body) = answer.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        tracing::error!("the broker's answer to a batch is larger than the gateway can merge");
        return ProblemDetails::internal().into_response();
    };
    let payload = serde_json::from_slice::<Value>(&bytes).ok();
    let (success, mut errors) = match (parts.status, payload) {
        (StatusCode::MULTI_STATUS, Some(Value::Object(mut result))) => {
            let mut members = |name: &str| match result.remove(name) {
                Some(Value::Array(values)) => values,
                _ => Vec::new(),
            };
            (members("success"), members("errors"))
        }
        (status, Some(Value::Array(ids))) if status.is_success() => (ids, Vec::new()),
        (status, _) if status.is_success() => (
            forwarded.into_iter().map(Value::String).collect(),
            Vec::new(),
        ),
        (status, payload) => {
            let problem = payload.filter(Value::is_object).unwrap_or_else(|| {
                serde_json::json!(ProblemDetails::new(
                    status.as_u16(),
                    "broker-failure",
                    "The context broker could not answer",
                ))
            });
            let errors = forwarded
                .into_iter()
                .map(|id| serde_json::json!({ "entityId": id, "error": problem }))
                .collect();
            (Vec::new(), errors)
        }
    };
    errors.extend(refusal_entries(refused));
    batch_result(success, errors)
}

/// 400 naming the type, when an entity's type is not a class of the space's one model (DM-61).
///
/// The model is published on the schema surface, so naming the missing type tells the caller
/// nothing a grant hides; it is the one thing they need to fix the write. A space that names no
/// model yet is not narrowed here.
fn undeclared_type(entity: &Value, endpoint: &Endpoint) -> Option<ProblemDetails> {
    let declared = endpoint.declared_types.as_ref()?;
    let object = entity.as_object()?;
    let types: Vec<&str> = match object.get("type").or_else(|| object.get("@type"))? {
        Value::String(one) => vec![one.as_str()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => return Some(ProblemDetails::bad_request().with_detail("entity type is not a string")),
    };
    let missing = types.into_iter().find(|kind| !declared.declares(kind))?;
    tracing::info!(slug = %endpoint.slug, entity_type = missing, model = %declared.model, "write refused: type not in the model");
    Some(ProblemDetails::bad_request().with_detail(format!(
        "entity type `{missing}` is not a class of the space's data model `{}` (DM-61)",
        declared.model
    )))
}

/// Whether the broker's `404` says the tenant does not exist (CIM 009 5.5.2 `NonexistentTenant`)
/// rather than that the thing asked for does not.
pub(crate) fn names_no_tenant(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).is_ok_and(|problem| {
        problem
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.rsplit('/').next() == Some("NonexistentTenant"))
    })
}

/// What an empty space answers `operation` with, or `None` for an operation that names one
/// thing, whose miss stays a miss (EP-22, CIM 009 5.2.24, 5.2.27).
pub(crate) fn empty_space(operation: Operation) -> Option<Value> {
    // A list document's `id` only has to be a URI of its own; the broker mints a UUID, this a
    // number that differs between two answers.
    let minted = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    };
    match operation {
        Operation::QueryEntity
        | Operation::QueryBatch
        | Operation::QueryTemporal
        | Operation::RetrieveEntityTypeDetails
        | Operation::RetrieveAttrTypeDetails => Some(Value::Array(Vec::new())),
        Operation::RetrieveEntityTypes => Some(serde_json::json!({
            "id": format!("urn:ngsi-ld:EntityTypeList:{:x}", minted()),
            "type": "EntityTypeList",
            "typeList": [],
        })),
        Operation::RetrieveAttrTypes => Some(serde_json::json!({
            "id": format!("urn:ngsi-ld:AttributeList:{:x}", minted()),
            "type": "AttributeList",
            "attributeList": [],
        })),
        _ => None,
    }
}

#[cfg(test)]
mod object_query_tests {
    use super::object_query;

    /// F-PLT2-010: an object that would close the string of the `q` is refused, a URN passes.
    #[test]
    fn only_a_urn_reaches_the_query() {
        assert_eq!(
            object_query("refStation", "urn:ngsi-ld:Station:a")
                .ok()
                .as_deref(),
            Some("refStation==\"urn:ngsi-ld:Station:a\"")
        );
        for object in ["urn:ngsi-ld:X:a\"|b==\"c", "urn:ngsi-ld:X:a\\", ""] {
            assert!(object_query("refStation", object).is_err(), "{object}");
        }
    }
}
