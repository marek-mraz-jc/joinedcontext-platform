//! Every environment variable a service reads is explained where it is read (T-2140, OPS-27).
//!
//! `docs/Deployment/13-configuration-reference.md` is generated from these doc comments: the
//! description of `JC_GATEWAY_BROKER_URL` an operator reads is the rustdoc of the field that
//! reads it. A variable added without one leaves a name with no meaning in the reference, and
//! the docs lane only notices after the code is merged — this notices in the crate's own fast
//! lane, before it is.
//!
//! This is a source scan, not a runtime check: it reads the workspace's `.rs` files the way
//! `docs/scripts/generate-config-reference.py` does, so the two cannot disagree about what
//! counts as a read.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The crates that read their configuration from the environment.
const SERVICES: &[&str] = &["context-gateway", "agent-proxy", "functions", "jcctl"];

/// Names the platform writes into a workload rather than reads: the pipeline runner's own
/// environment. They are documented with the runner, and jcctl only validates them.
const INJECTED: &[&str] = &["JC_ORG_DOMAIN", "JC_SOURCE_SPACE", "JC_SPACE"];

/// What a name has to look like to hold a credential rather than an address.
fn is_secret(name: &str) -> bool {
    if name.ends_with("_URL") || name.ends_with("_FILE") {
        return false;
    }
    name.contains("SECRET") || name.contains("TOKEN") || name.ends_with("_KEY")
}

fn workspace() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("jc-core sits two levels below the workspace root")
        .to_path_buf();
    assert!(
        root.join("crates/context-gateway/src").is_dir(),
        "{} is not the workspace root: the layout moved and this test is looking in the wrong \
         place, which would make it pass by finding nothing",
        root.display(),
    );
    root
}

/// Every `.rs` file of a directory, tests excluded.
fn sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                continue;
            }
            found.extend(sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// The source up to its test module: a test sets variables rather than reading them.
fn without_tests(text: &str) -> &str {
    match text.find("#[cfg(test)]") {
        Some(at) => &text[..at],
        None => text,
    }
}

/// Every `JC_…`/`PORTAL_…` name on a line, with whether it is written as a string literal.
fn names_on(line: &str) -> Vec<(String, bool)> {
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < line.len() {
        let rest = &line[at..];
        let Some(start) = ["JC_", "PORTAL_"]
            .iter()
            .filter_map(|prefix| rest.find(prefix))
            .min()
        else {
            break;
        };
        let begin = at + start;
        let end = line[begin..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(line.len(), |offset| begin + offset);
        let name = &line[begin..end];
        // A prefix (`JC_SPACE_`) is a family of names, not one variable.
        if !name.ends_with('_') && name.len() > 3 {
            let quoted =
                begin > 0 && bytes[begin - 1] == b'"' && end < bytes.len() && bytes[end] == b'"';
            found.push((name.to_owned(), quoted));
        }
        at = end.max(begin + 1);
    }
    found
}

fn is_doc(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("///") || trimmed.starts_with("//!")
}

/// What each crate reads, and everything the workspace documents.
fn read_and_documented() -> (BTreeMap<String, BTreeSet<String>>, BTreeSet<String>) {
    let root = workspace();
    let mut read: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut documented = BTreeSet::new();

    for crate_name in SERVICES.iter().chain(["jc-core"].iter()) {
        let source_root = root.join("crates").join(crate_name).join("src");
        for path in sources(&source_root) {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            for line in without_tests(&text).lines() {
                if is_doc(line) {
                    for (name, _) in names_on(line) {
                        documented.insert(name);
                    }
                    continue;
                }
                if line.trim_start().starts_with("//") {
                    continue;
                }
                // `{ "name": "JC_X", "value": … }` writes the variable into a workload.
                let writes = line.contains("\"name\"");
                for (name, quoted) in names_on(line) {
                    if !quoted || writes || INJECTED.contains(&name.as_str()) {
                        continue;
                    }
                    if SERVICES.contains(crate_name) {
                        read.entry(name)
                            .or_default()
                            .insert((*crate_name).to_owned());
                    }
                }
            }
        }
    }
    (read, documented)
}

/// OPS-27: the reference page says what each variable is, and it can only say it if the code
/// does. A variable read and nowhere documented is one the page would print as a bare name.
#[test]
fn every_variable_a_service_reads_is_documented_where_it_is_read() {
    let (read, documented) = read_and_documented();
    assert!(
        read.len() > 30,
        "only {} variable(s) found: the scan is looking in the wrong place, which would make \
         this test pass by finding nothing",
        read.len(),
    );

    let mut undocumented = Vec::new();
    for (name, crates) in &read {
        if !documented.contains(name) {
            let where_read = crates.iter().cloned().collect::<Vec<_>>().join(", ");
            undocumented.push(format!("{name} (read by {where_read})"));
        }
    }
    assert!(
        undocumented.is_empty(),
        "{} variable(s) are read and no doc comment says what they are:\n  {}\n\
         Document each one where it is read: the configuration reference is generated from \
         those sentences.",
        undocumented.len(),
        undocumented.join("\n  "),
    );
}

/// OPS-27: a secret is resolved from a `secretRef` and never written down. Reading one
/// straight into a log line writes it into the cluster's log store, which is the one place a
/// credential is hardest to take back out of.
#[test]
fn no_secret_is_read_straight_into_a_log_line() {
    let root = workspace();
    let logs = [
        "tracing::trace!",
        "tracing::debug!",
        "tracing::info!",
        "tracing::warn!",
        "tracing::error!",
        "println!",
        "eprintln!",
        "dbg!",
    ];
    let reads = ["env::var(", "env::var_os(", "lookup(", "var(", "var_os("];

    let mut leaked = Vec::new();
    for crate_name in SERVICES {
        let source_root = root.join("crates").join(crate_name).join("src");
        for path in sources(&source_root) {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            for (number, line) in without_tests(&text).lines().enumerate() {
                if is_doc(line) || !logs.iter().any(|macro_name| line.contains(macro_name)) {
                    continue;
                }
                for (name, quoted) in names_on(line) {
                    let read_here = reads.iter().any(|call| {
                        line.find(call)
                            .is_some_and(|at| line[at..].contains(&format!("\"{name}\"")))
                    });
                    if quoted && read_here && is_secret(&name) {
                        leaked.push(format!("{}:{}: {name}", path.display(), number + 1));
                    }
                }
            }
        }
    }
    assert!(
        leaked.is_empty(),
        "a secret's value is read into a log line:\n  {}",
        leaked.join("\n  "),
    );
}

/// The two cases above are only worth anything if the scan reads a source the way it thinks it
/// does. This holds the reader itself: what counts as a name, a literal, a doc comment and a
/// secret.
#[test]
fn the_source_reader_knows_a_read_from_a_mention() {
    assert_eq!(
        names_on(r#"    let bind = lookup("JC_PORTAL_BIND");"#),
        vec![("JC_PORTAL_BIND".to_owned(), true)],
    );
    assert_eq!(
        names_on("/// The address to listen on (`JC_GATEWAY_BIND`, default)."),
        vec![("JC_GATEWAY_BIND".to_owned(), false)],
    );
    assert_eq!(
        names_on(r#"{ "name": "JC_APP_NAME", "value": app }"#),
        vec![("JC_APP_NAME".to_owned(), true)],
    );
    // A prefix is a family of names; `JC_` alone is not a name at all.
    assert!(names_on(r#".strip_prefix("JC_SPACE_")"#).is_empty());
    assert!(is_doc("    /// a field"));
    assert!(is_doc("//! a module"));
    assert!(!is_doc("    // an ordinary comment"));
    assert!(is_secret("JC_GITEA_TOKEN"));
    assert!(is_secret("JC_OIDC_CLIENT_SECRET"));
    assert!(is_secret("JC_MODEL_KEY"));
    assert!(
        !is_secret("JC_OIDC_TOKEN_URL"),
        "an address is not a secret"
    );
    assert!(!is_secret("JC_MODEL_KEY_FILE"), "a path is not the value");
    assert!(!is_secret("JC_GATEWAY_BIND"));
    assert_eq!(without_tests("a\n#[cfg(test)]\nb"), "a\n");
}
