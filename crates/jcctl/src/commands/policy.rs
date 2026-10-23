//! `jcctl policy export` and `jcctl policy import`: a Policy manifest as an ODRL 2.2 `Set` in the
//! `ngsi-ld:` profile and back (ADR-N-057 decision 4, T-5039). The mapping is jc-core's
//! [`policy_odrl`], which the Portal's Policy form reads too; the YAML file stays the source.

use jc_core::kinds::policy_odrl::{self, Names};
use jc_core::kinds::Policy;
use std::path::{Path, PathBuf};

/// What `jcctl policy import` writes, and where.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// The ODRL JSON-LD file.
    pub source: PathBuf,
    /// Project, space and name where the `Set`'s `uid` does not name them.
    pub names: Names,
    /// The manifest file to create; stdout when absent.
    pub out: Option<PathBuf>,
}

impl ImportOptions {
    /// `<file.jsonld> [--project <slug>] [--space <name>] [--name <name>] [--out <file.yaml>]`.
    pub fn parse(words: &[&str]) -> Option<Self> {
        let (source, mut rest) = words.split_first()?;
        let mut options = Self {
            source: PathBuf::from(source),
            ..Self::default()
        };
        while let [flag, value, tail @ ..] = rest {
            let value = Some((*value).to_owned());
            match *flag {
                "--project" => options.names.project = value,
                "--space" => options.names.space = value,
                "--name" => options.names.name = value,
                "--out" => options.out = value.map(PathBuf::from),
                _ => return None,
            }
            rest = tail;
        }
        (rest.is_empty() && !source.starts_with('-')).then_some(options)
    }
}

/// The Policy manifest at `path` as an ODRL `Set`, pretty JSON.
pub fn export(path: &Path) -> Result<String, String> {
    let text = read(path)?;
    let policy = Policy::from_yaml(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    policy
        .validate()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let set = policy_odrl::render(&policy).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::to_string_pretty(&set).map_err(|e| e.to_string())
}

/// The Policy manifest the ODRL `Set` says, formatted as `jcctl fmt` writes it; written to
/// `--out` (never over an existing file) or returned for stdout.
pub fn import(options: &ImportOptions) -> Result<String, String> {
    let source = options.source.display();
    let document: serde_json::Value =
        serde_json::from_str(&read(&options.source)?).map_err(|e| format!("{source}: {e}"))?;
    let policy = policy_odrl::import(&document, &options.names)
        .map_err(|e| format!("{source}: refused at {e}"))?;
    let yaml = policy.to_yaml().map_err(|e| e.to_string())?;
    let yaml = crate::fmt::format_file(Path::new("policy.yaml"), &yaml)
        .map_err(|e| format!("formatting the manifest: {e}"))?;
    let Some(out) = &options.out else {
        return Ok(yaml);
    };
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .and_then(|mut file| std::io::Write::write_all(&mut file, yaml.as_bytes()))
        .map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(format!("wrote {}", out.display()))
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: public-air\n  namespace: ovzdusie\nspec:\n  contextSpaceRef: { kind: ContextSpace, name: ovzdusie }\n  assigner: did:web:banskabystrica.sk\n  assignee: { kind: role, id: public }\n  operations: [queryEntity]\n  information:\n    - entities:\n        - type: AirQualityObserved\n";

    #[test]
    fn export_then_import_writes_the_same_file_and_never_over_one() {
        let dir = std::env::temp_dir().join(format!("jcctl-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let manifest = dir.join("public-air.yaml");
        std::fs::write(&manifest, POLICY).expect("manifest");
        let set = dir.join("set.jsonld");
        std::fs::write(&set, export(&manifest).expect("exports")).expect("set");
        let options = ImportOptions::parse(&[set.to_str().expect("utf-8")]).expect("parses");
        assert_eq!(import(&options).expect("imports"), POLICY);
        let out = ImportOptions {
            out: Some(manifest.clone()),
            ..options
        };
        let refused = import(&out).expect_err("existing file");
        assert!(refused.contains("exists"), "{refused}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_refusal_names_the_member() {
        let dir = std::env::temp_dir().join(format!("jcctl-policy-refusal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let set = dir.join("offer.jsonld");
        let offer = serde_json::json!({"@context": policy_odrl::CONTEXTS, "@type": "Offer",
            "assigner": "did:web:x.example", "permission": []});
        std::fs::write(&set, offer.to_string()).expect("set");
        let options = ImportOptions::parse(&[
            set.to_str().expect("utf-8"),
            "--project",
            "p",
            "--space",
            "s",
            "--name",
            "n",
        ])
        .expect("parses");
        let refused = import(&options).expect_err("an Offer");
        assert!(refused.contains("refused at @type"), "{refused}");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn options_refuse_an_unknown_flag_or_a_missing_value() {
        assert!(ImportOptions::parse(&["set.jsonld", "--force", "x"]).is_none());
        assert!(ImportOptions::parse(&["set.jsonld", "--name"]).is_none());
        assert!(ImportOptions::parse(&["--name", "x"]).is_none());
        let options =
            ImportOptions::parse(&["s.jsonld", "--name", "a", "--out", "a.yaml"]).expect("parses");
        assert_eq!(options.names.name.as_deref(), Some("a"));
        assert_eq!(options.out, Some(PathBuf::from("a.yaml")));
    }
}
