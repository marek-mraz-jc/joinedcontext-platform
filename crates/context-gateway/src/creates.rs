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

/// The creates each Endpoint has taken today (UTC), keyed by slug.
///
/// A create reserves its slot before the broker is asked, so concurrent creates cannot pass
/// the cap together, and gives it back when the broker refuses it.
// ponytail: one process-wide count; a second gateway replica counts on its own, so the
// effective cap is perDay × replicas until the count moves into the broker or Redis.
#[derive(Debug, Default)]
pub struct DailyCreates {
    counts: Mutex<HashMap<String, (u64, u32)>>,
}

impl DailyCreates {
    /// Takes one of the day's `per_day` creates for `slug` at `now` (Unix seconds), or the
    /// seconds until midnight UTC when they are spent.
    pub fn reserve(&self, slug: &str, per_day: u32, now: u64) -> Result<(), u64> {
        let day = now / DAY;
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let entry = counts.entry(slug.to_owned()).or_insert((day, 0));
        if entry.0 != day {
            *entry = (day, 0);
        }
        if entry.1 >= per_day {
            return Err(DAY - now % DAY);
        }
        entry.1 += 1;
        Ok(())
    }

    /// Gives back a slot `reserve` took at `now` whose create the broker did not make.
    pub fn release(&self, slug: &str, now: u64) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = counts.get_mut(slug) {
            if entry.0 == now / DAY {
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
        assert_eq!(creates.reserve("a", 2, noon), Ok(()));
        assert_eq!(creates.reserve("a", 2, noon), Ok(()));
        assert_eq!(creates.reserve("a", 2, noon), Err(DAY / 2));
        assert_eq!(creates.reserve("b", 2, noon), Ok(()));
        creates.release("a", noon);
        assert_eq!(creates.reserve("a", 2, noon), Ok(()));
        assert_eq!(creates.reserve("a", 2, 11 * DAY), Ok(()));
    }
}
