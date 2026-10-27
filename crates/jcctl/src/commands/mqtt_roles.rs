//! `jcctl migrate mqtt-roles --repo-dir <dir>`: moves who holds a broker role from the
//! deprecated `MqttBroker` `roles[].serviceAccounts` lists to a RoleBinding
//! `mqtt/<broker>/<role>` on each listed ServiceAccount (ADR-N-048 §2.13, T-5872).
//!
//! Access stays what it was: each listed account gets exactly the binding for the role that
//! listed it, scoped to its own project, with no validity, and the list is removed only once
//! every name on it has its binding. A broker naming an account no manifest declares keeps its
//! list and is reported. Files are rewritten in place through the canonical formatter; the
//! checkout is git, so the change is reviewed as a diff before it is proposed.

use jc_manifest::loader::Repository;
use serde_norway::{Mapping, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What the migration changed and what it left.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Each file rewritten, repository-relative.
    pub rewritten: Vec<PathBuf>,
    /// Each list left in place, and why.
    pub left: Vec<String>,
}

/// Rewrites the repository at `dir`; an error names the file that could not be read or written.
pub fn run(dir: &Path) -> Result<Outcome, String> {
    let repo = Repository::load(dir).map_err(|e| e.to_string())?;
    let mut accounts: BTreeMap<(String, String), PathBuf> = BTreeMap::new();
    let mut brokers = Vec::new();
    for (id, resource) in repo.iter() {
        let project = id.namespace.clone().unwrap_or_default();
        match id.kind.as_str() {
            "ServiceAccount" => {
                accounts.insert((project, id.name.clone()), resource.path.clone());
            }
            "MqttBroker" => brokers.push((project, id.name.clone(), resource.path.clone())),
            _ => {}
        }
    }
    let mut files: BTreeMap<PathBuf, Value> = BTreeMap::new();
    let mut outcome = Outcome::default();
    for (project, broker, path) in brokers {
        let mut manifest = read(dir, &path, &mut files)?;
        let Some(roles) = manifest
            .get_mut("spec")
            .and_then(|spec| spec.get_mut("roles"))
            .and_then(Value::as_sequence_mut)
        else {
            continue;
        };
        let mut changed = false;
        for role in roles.iter_mut().filter_map(Value::as_mapping_mut) {
            let name = role
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let Some(listed) = role.get("serviceAccounts").and_then(Value::as_sequence) else {
                continue;
            };
            let listed: Vec<String> = listed
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let missing: Vec<&String> = listed
                .iter()
                .filter(|account| !accounts.contains_key(&(project.clone(), (*account).clone())))
                .collect();
            if !missing.is_empty() {
                outcome.left.push(format!(
                    "{}: role `{name}` lists {missing:?}, which no ServiceAccount of project `{project}` declares; add them or remove them, then run again",
                    path.display()
                ));
                continue;
            }
            for account in &listed {
                let at = &accounts[&(project.clone(), account.clone())];
                let mut doc = read(dir, at, &mut files)?;
                bind(&mut doc, &format!("mqtt/{broker}/{name}"), &project);
                files.insert(at.clone(), doc);
            }
            role.remove("serviceAccounts");
            changed = true;
        }
        if changed {
            files.insert(path.clone(), manifest);
        }
    }
    for (path, doc) in files {
        let before = std::fs::read_to_string(dir.join(&path))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let text = serde_norway::to_string(&doc).map_err(|e| format!("{}: {e}", path.display()))?;
        let text =
            jc_core::yaml_fmt::format(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if text != before {
            std::fs::write(dir.join(&path), text)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            outcome.rewritten.push(path);
        }
    }
    Ok(outcome)
}

/// The manifest at `path`, from the files already changed or else from disk; a file of several
/// documents is not rewritten.
fn read(dir: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Value>) -> Result<Value, String> {
    if let Some(doc) = files.get(path) {
        return Ok(doc.clone());
    }
    let text =
        std::fs::read_to_string(dir.join(path)).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_norway::from_str(&text).map_err(|e| {
        format!(
            "{}: {e} (a file of several documents is migrated by hand)",
            path.display()
        )
    })
}

/// Adds the binding `role` scoped to `project` to the account `doc`, unless it holds it already.
fn bind(doc: &mut Value, role: &str, project: &str) {
    let Some(spec) = doc.get_mut("spec").and_then(Value::as_mapping_mut) else {
        return;
    };
    let roles = spec
        .entry(Value::from("roles"))
        .or_insert_with(|| Value::Sequence(Vec::new()));
    let Some(roles) = roles.as_sequence_mut() else {
        return;
    };
    let held = roles.iter().any(|binding| {
        binding.get("role").and_then(Value::as_str) == Some(role)
            && binding
                .get("scope")
                .and_then(|s| s.get("project"))
                .and_then(Value::as_str)
                == Some(project)
    });
    if held {
        return;
    }
    let mut scope = Mapping::new();
    scope.insert("project".into(), project.into());
    let mut binding = Mapping::new();
    binding.insert("role".into(), role.into());
    binding.insert("scope".into(), Value::Mapping(scope));
    roles.push(Value::Mapping(binding));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        std::fs::write(path, body).expect("written");
    }

    fn account(name: &str) -> String {
        format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: {name}\n  namespace: air\nspec:\n  owner:\n    user: demo.steward\n  purpose: sensors\n  roles:\n    - role: space-writer\n      scope:\n        project: air\n  credentials:\n    - kind: oauth-client\n      name: default\n")
    }

    #[test]
    fn each_listed_account_gets_its_binding_and_the_list_goes() {
        let dir = std::env::temp_dir().join("jcctl-mqtt-roles");
        let _ = std::fs::remove_dir_all(&dir);
        write(
            &dir,
            "projects/air/access/serviceaccounts/fleet.yaml",
            &account("fleet"),
        );
        write(
            &dir,
            "projects/air/mqttbrokers/air.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: MqttBroker\nmetadata:\n  name: air\n  namespace: air\nspec:\n  roles:\n    - name: sensor\n      serviceAccounts: [fleet]\n      publish: [\"air/${clientid}/pm10\"]\n    - name: camera\n      serviceAccounts: [ghost]\n      publish: [\"cams/${clientid}\"]\n",
        );
        let outcome = run(&dir).expect("runs");
        assert_eq!(outcome.rewritten.len(), 2, "{outcome:?}");
        assert_eq!(outcome.left.len(), 1);
        assert!(outcome.left[0].contains("ghost"), "{outcome:?}");
        let broker =
            std::fs::read_to_string(dir.join("projects/air/mqttbrokers/air.yaml")).expect("read");
        assert!(
            broker.contains("[ghost]") && broker.matches("serviceAccounts").count() == 1,
            "{broker}"
        );
        let fleet =
            std::fs::read_to_string(dir.join("projects/air/access/serviceaccounts/fleet.yaml"))
                .expect("read");
        assert!(fleet.contains("role: mqtt/air/sensor"), "{fleet}");
        // The rewritten repository still validates, and a second run changes nothing.
        let report = jc_manifest::validate::run(&dir);
        assert!(
            report
                .findings
                .iter()
                .all(|f| !f.to_string().contains("fleet")),
            "{:?}",
            report.findings
        );
        assert_eq!(
            run(&dir).expect("runs again").rewritten,
            Vec::<PathBuf>::new()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
