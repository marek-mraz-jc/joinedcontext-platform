//! Which Apps this shard serves (ADR-N-044 §2.1, AP-143): the placement the reconciler records,
//! rendered as one JSON file per shard and mounted into its pods. A shard serves the Apps of its
//! own file and no other; an App moves only when the reconciler moves it.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// One App placed on this shard.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Placed {
    /// The App's name, the `{name}` of `/apps/{name}/api/*`.
    pub name: String,
    /// The App's id: its database role and schema are `app_<id>`, its prefix `apps/<shard>/<id>/`.
    pub id: String,
    /// The project it belongs to, whose concurrency it shares.
    pub tenant: String,
    /// `sha256:<64 hex>` of the component its build recorded.
    pub digest: String,
}

/// The placement file of one shard.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Placement {
    pub shard: String,
    pub apps: Vec<Placed>,
}

/// A DNS-label-like id: lowercase letters, digits and `-`/`_`, 1 to 40 characters. It is part of a
/// database role name and a storage prefix, so nothing else may reach either.
pub fn is_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// `sha256:` and 64 lowercase hex digits.
pub fn is_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

impl Placement {
    /// The file read and checked: the shard this pod is, every id and digest well formed, no name
    /// or id twice. A file for another shard is refused, so a pod never serves Apps not its own.
    pub fn parse(text: &str, shard: &str) -> Result<HashMap<String, Placed>, String> {
        let placement: Placement =
            serde_json::from_str(text).map_err(|err| format!("the placement file: {err}"))?;
        if placement.shard != shard {
            return Err(format!(
                "the placement file is shard {}'s, and this is shard {shard}",
                placement.shard
            ));
        }
        let mut apps = HashMap::new();
        let mut ids = std::collections::HashSet::new();
        for app in placement.apps {
            if !is_id(&app.id) || !is_id(&app.name) || !is_id(&app.tenant) {
                return Err(format!("the placement of `{}`: names and ids are lowercase letters, digits, `-` and `_`", app.name));
            }
            if !is_digest(&app.digest) {
                return Err(format!(
                    "the placement of `{}`: the digest is not sha256:<64 hex>",
                    app.name
                ));
            }
            if !ids.insert(app.id.clone()) {
                return Err(format!("the id `{}` is placed twice", app.id));
            }
            if apps.insert(app.name.clone(), app.clone()).is_some() {
                return Err(format!("the name `{}` is placed twice", app.name));
            }
        }
        Ok(apps)
    }

    /// The file at `path`, read now.
    pub fn read(path: &Path, shard: &str) -> Result<HashMap<String, Placed>, String> {
        let text =
            std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
        Self::parse(&text, shard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn file(apps: &str) -> String {
        format!(r#"{{"shard": "s1", "apps": [{apps}]}}"#)
    }

    #[test]
    fn a_shard_reads_its_own_apps() {
        let apps = Placement::parse(
            &file(&format!(
                r#"{{"name": "notes", "id": "a1", "tenant": "helsinki", "digest": "{D}"}}"#
            )),
            "s1",
        )
        .unwrap();
        assert_eq!(apps["notes"].id, "a1");
    }

    #[test]
    fn another_shards_file_a_bad_id_or_a_twin_is_refused() {
        let one = format!(r#"{{"name": "notes", "id": "a1", "tenant": "t", "digest": "{D}"}}"#);
        assert!(Placement::parse(&file(&one), "s2")
            .unwrap_err()
            .contains("shard s1"));
        for bad in [
            one.replace("\"a1\"", "\"a1; drop\""),
            one.replace("\"a1\"", "\"A1\""),
            one.replace("\"a1\"", "\"../x\""),
            one.replace("\"a1\"", "\"\""),
            one.replace(D, "sha256:xyz"),
            one.replace(D, &D.to_uppercase()),
        ] {
            assert!(Placement::parse(&file(&bad), "s1").is_err(), "{bad}");
        }
        let twin_id = format!("{one}, {}", one.replace("notes", "other"));
        assert!(Placement::parse(&file(&twin_id), "s1")
            .unwrap_err()
            .contains("id `a1`"));
        let twin_name = format!("{one}, {}", one.replace("\"a1\"", "\"a2\""));
        assert!(Placement::parse(&file(&twin_name), "s1")
            .unwrap_err()
            .contains("name `notes`"));
        assert!(Placement::parse(r#"{"shard": "s1", "apps": [], "extra": 1}"#, "s1").is_err());
    }
}
