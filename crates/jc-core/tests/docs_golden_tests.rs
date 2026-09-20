//! T-0002: every manifest example printed in the docs must deserialize and validate.
//!
//! `tests/golden/` holds the YAML blocks copied verbatim out of the `joinedcontext-docs`
//! repository (file name: `{n}-{Kind}-{doc}.yaml`). Adding a kind or changing a doc example
//! means dropping the new block in here; the test dispatches through
//! [`jc_core::registry::validate_yaml`], so it needs no per-kind code.

use std::fs;
use std::path::Path;

#[test]
fn every_documented_manifest_parses_and_validates() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut checked = 0;

    for entry in fs::read_dir(&dir).expect("tests/golden exists") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let yaml = fs::read_to_string(&path).expect("readable golden manifest");
        let kind = yaml
            .lines()
            .find_map(|l| l.strip_prefix("kind: "))
            // doc examples annotate the line: `kind: ContextSpace # or DataModel, Policy, …`
            .and_then(|k| k.split('#').next())
            .map(|k| k.trim())
            .unwrap_or_else(|| panic!("{} has no `kind:` line", path.display()));

        match jc_core::registry::validate_yaml(kind, &yaml) {
            None => panic!("{}: kind `{kind}` is not in the registry", path.display()),
            Some(Err(e)) => panic!("{}: {e}", path.display()),
            Some(Ok(())) => checked += 1,
        }
    }

    assert!(
        checked >= 15,
        "expected the documented manifests, got {checked}"
    );
}
