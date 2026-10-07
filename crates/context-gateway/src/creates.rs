//! Creates through an Endpoint that takes them from callers it does not know: a public form
//! (EP-97). The gateway mints the entity's id, so a caller can neither choose one nor
//! overwrite an entity it guessed, and counts the day's creates against `spec.creates.perDay`.

use jc_core::ProblemDetails;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

const DAY: u64 = 86_400;

/// The body of a create with its `id` replaced by `urn:ngsi-ld:{Type}:{uuid}`.
///
/// The type is read from the body; an IRI or CURIE gives its local name, the part the URN's
/// type segment holds. A body that is not one entity with a string `type` is refused.
pub fn minted(sent: &[u8]) -> Result<Vec<u8>, Box<ProblemDetails>> {
    let refused =
        |detail: &str| Box::new(ProblemDetails::bad_request().with_detail(detail.to_owned()));
    let mut entity: Value =
        serde_json::from_slice(sent).map_err(|_| refused("the body is not one JSON entity"))?;
    let Some(object) = entity.as_object_mut() else {
        return Err(refused("this endpoint takes one entity per request"));
    };
    let entity_type = object
        .get("type")
        .and_then(Value::as_str)
        .and_then(|t| t.rsplit(['/', '#', ':']).next())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| refused("the entity names no type"))?
        .to_owned();
    let id = format!("urn:ngsi-ld:{entity_type}:{}", uuid_v4()?);
    object.insert("id".to_owned(), Value::String(id));
    serde_json::to_vec(&entity).map_err(|_| Box::new(ProblemDetails::internal()))
}

/// A random RFC 9562 version 4 UUID from the system's generator.
fn uuid_v4() -> Result<String, Box<ProblemDetails>> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Box::new(ProblemDetails::internal()))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// Whose part of the day a refused create ran out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spent {
    /// The Endpoint's whole `perDay`.
    Form,
    /// This caller's tenth of it (EP-97, T-3285).
    Caller,
}

/// The most callers kept for one day before older days are dropped.
const MAX_CALLERS: usize = 100_000;

/// One caller's share of a day's `per_day`: a tenth, rounded up (EP-97).
pub fn share(per_day: u32) -> u32 {
    per_day.div_ceil(10).max(1)
}

/// The creates each Endpoint has taken today (UTC), keyed by slug, and each caller's part of
/// them, keyed by slug and caller ([`crate::middleware::rate_limit::caller_key`]).
///
/// A create reserves its slot before the broker is asked, so concurrent creates cannot pass
/// the cap together, and gives it back when the broker refuses it.
// ponytail: one process-wide count; a second gateway replica counts on its own, so the
// effective cap is perDay × replicas until the count moves into the broker or Redis.
#[derive(Debug, Default)]
pub struct DailyCreates {
    counts: Mutex<HashMap<String, (u64, u32)>>,
    callers: Mutex<HashMap<(String, String), (u64, u32)>>,
}

impl DailyCreates {
    /// Takes one of the day's `per_day` creates for `slug` by `caller` at `now` (Unix seconds),
    /// or the seconds until midnight UTC and whose part is spent: the caller's own share is
    /// judged first, so one caller never empties the form for everyone.
    pub fn reserve(
        &self,
        slug: &str,
        caller: &str,
        per_day: u32,
        now: u64,
    ) -> Result<(), (u64, Spent)> {
        let day = now / DAY;
        let wait = DAY - now % DAY;
        let mut callers = self.callers.lock().unwrap_or_else(|e| e.into_inner());
        if callers.len() >= MAX_CALLERS {
            callers.retain(|_, (when, _)| *when == day);
        }
        let mine = callers
            .entry((slug.to_owned(), caller.to_owned()))
            .or_insert((day, 0));
        if mine.0 != day {
            *mine = (day, 0);
        }
        if mine.1 >= share(per_day) {
            return Err((wait, Spent::Caller));
        }
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let entry = counts.entry(slug.to_owned()).or_insert((day, 0));
        if entry.0 != day {
            *entry = (day, 0);
        }
        if entry.1 >= per_day {
            return Err((wait, Spent::Form));
        }
        entry.1 += 1;
        mine.1 += 1;
        Ok(())
    }

    /// Gives back a slot `reserve` took at `now` whose create the broker did not make.
    pub fn release(&self, slug: &str, caller: &str, now: u64) {
        let day = now / DAY;
        let mut callers = self.callers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mine) = callers.get_mut(&(slug.to_owned(), caller.to_owned())) {
            if mine.0 == day {
                mine.1 = mine.1.saturating_sub(1);
            }
        }
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = counts.get_mut(slug) {
            if entry.0 == day {
                entry.1 = entry.1.saturating_sub(1);
            }
        }
    }
}

/// Unix seconds now; the epoch when the clock reads earlier, which only makes the day 0.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_replaces_the_callers_id() {
        let body =
            br#"{"id":"urn:ngsi-ld:Report:mine","type":"https://uri.fiware.org/ns/Report","x":1}"#;
        let out: Value = serde_json::from_slice(&minted(body).expect("minted")).expect("json");
        let id = out["id"].as_str().expect("id");
        assert!(id.starts_with("urn:ngsi-ld:Report:"), "{id}");
        assert_ne!(id, "urn:ngsi-ld:Report:mine");
        assert_eq!(id.len(), "urn:ngsi-ld:Report:".len() + 36);
        assert_eq!(out["x"], 1);
        assert_ne!(minted(body).expect("again"), minted(body).expect("again"));
    }

    #[test]
    fn minted_refuses_what_is_not_one_typed_entity() {
        for body in [
            &b"[]"[..],
            b"{}",
            br#"{"type":3}"#,
            br#"{"type":"x:"}"#,
            b"nope",
        ] {
            assert_eq!(minted(body).expect_err("refused").status, 400);
        }
    }

    #[test]
    fn the_cap_counts_per_slug_per_day_and_gives_back() {
        let creates = DailyCreates::default();
        let noon = 10 * DAY + DAY / 2;
        // Two callers, a cap of 2: each one's share is 1.
        assert_eq!(creates.reserve("a", "x", 2, noon), Ok(()));
        assert_eq!(creates.reserve("a", "y", 2, noon), Ok(()));
        assert_eq!(
            creates.reserve("a", "z", 2, noon),
            Err((DAY / 2, Spent::Form))
        );
        assert_eq!(creates.reserve("b", "x", 2, noon), Ok(()));
        creates.release("a", "y", noon);
        assert_eq!(creates.reserve("a", "z", 2, noon), Ok(()));
        assert_eq!(creates.reserve("a", "x", 2, 11 * DAY), Ok(()));
    }

    /// T-3285: one caller takes a tenth of the day and no more, so a second caller still
    /// creates after the first ran out; a refused create gives its share back.
    #[test]
    fn one_caller_never_spends_the_form_for_everyone() {
        let creates = DailyCreates::default();
        let noon = 10 * DAY + DAY / 2;
        assert_eq!(share(500), 50);
        assert_eq!(share(5), 1);
        assert_eq!(share(0), 1);
        for _ in 0..50 {
            assert_eq!(
                creates.reserve("form", "address:203.0.113.7", 500, noon),
                Ok(())
            );
        }
        assert_eq!(
            creates.reserve("form", "address:203.0.113.7", 500, noon),
            Err((DAY / 2, Spent::Caller))
        );
        assert_eq!(
            creates.reserve("form", "address:198.51.100.4", 500, noon),
            Ok(())
        );
        creates.release("form", "address:203.0.113.7", noon);
        assert_eq!(
            creates.reserve("form", "address:203.0.113.7", 500, noon),
            Ok(())
        );
        // The next day starts every share over.
        assert_eq!(
            creates.reserve("form", "address:203.0.113.7", 500, 11 * DAY),
            Ok(())
        );
    }
}
