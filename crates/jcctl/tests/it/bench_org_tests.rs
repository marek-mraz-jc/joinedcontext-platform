//! The scale bench's generated organization (T-4443): what it writes validates, and the same
//! size gives the same tree.

use jcctl::assemble::Directories;
use jcctl::bench_org;
use jcctl::commands::validate;

fn temp_dir(test: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("jcctl-bench-org-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn ten_endpoints_validate_and_the_tree_is_the_same_every_time() {
    let root = temp_dir("ten");
    let tree = bench_org::write(&root, 10).expect("the tree is written");
    assert_eq!(tree.slugs.len(), 10);
    let checkouts = Directories(
        std::fs::read_dir(&tree.projects)
            .expect("checkouts")
            .flatten()
            .map(|e| (e.file_name().into_string().expect("a name"), e.path()))
            .collect(),
    );
    let report = validate::run_assembled(&tree.org, &checkouts);
    assert!(report.is_valid(), "{:#?}", report.findings);
    assert!(
        report.checked >= 10 * 3,
        "{} manifests checked",
        report.checked
    );

    let again = bench_org::write(&temp_dir("again"), 10).expect("written again");
    assert_eq!(again.slugs, tree.slugs);
    let _ = std::fs::remove_dir_all(&root);
}
