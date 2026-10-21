use super::*;

fn fmt(src: &str) -> String {
    let out = format(src).unwrap_or_else(|e| panic!("{e}\n{src}"));
    assert_eq!(
        format(&out).as_deref(),
        Ok(out.as_str()),
        "not idempotent:\n{out}"
    );
    out
}

#[test]
fn manifest_sections_and_metadata_fields_take_the_canonical_order() {
    let src = "spec:\n  b: 1\n  a: 2\nmetadata:\n  labels:\n    x: z\n  name: p\n  namespace: n\nkind: Policy\napiVersion: joinedcontext.com/v1alpha1\n";
    let want = "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: p\n  namespace: n\n  labels: { x: z }\nspec:\n  b: 1\n  a: 2\n";
    assert_eq!(fmt(src), want);
}

#[test]
fn comments_stay_with_their_entries_and_the_header_stays_on_top() {
    let src = "# file header\n\nkind: Policy # the kind\napiVersion: v1\n# about spec\nspec:\n  # first\n  a: 1 # one\n\n\n  b: [x, z] # list\n";
    let want = "# file header\n\napiVersion: v1\nkind: Policy # the kind\n# about spec\nspec:\n  # first\n  a: 1 # one\n\n  b: [x, z] # list\n";
    assert_eq!(fmt(src), want);
}

#[test]
fn every_comment_of_the_input_is_in_the_output() {
    let src = "a:\n  - # on the dash\n    x: 1\n  # before an item\n  - y\nb: |\n  # not a comment\n# at the end\n";
    let out = fmt(src);
    for c in [
        "# on the dash",
        "# before an item",
        "# not a comment",
        "# at the end",
    ] {
        assert!(out.contains(c), "{c} lost:\n{out}");
    }
}

#[test]
fn comments_closing_a_block_stay_under_its_last_entry() {
    let src = "spec:\n    sources:\n    # BEGIN generated\n    - a\n    # END generated\n\n    other: 1\nlast: 2\n";
    let want = "spec:\n  sources:\n    # BEGIN generated\n    - a\n    # END generated\n\n  other: 1\nlast: 2\n";
    assert_eq!(fmt(src), want);
}

#[test]
fn short_scalar_maps_and_lists_go_flow_and_long_ones_block() {
    let long = "x".repeat(90);
    let src = format!("spec:\n  title:\n    en: Hi\n    sk: Ahoj\n  four:\n    a: 1\n    b: 2\n    c: 3\n    d: 4\n  tags:\n  - a\n  - b\n  wide:\n  - {long}\n  - {long}\n");
    let want = format!("spec:\n  title: {{ en: Hi, sk: Ahoj }}\n  four:\n    a: 1\n    b: 2\n    c: 3\n    d: 4\n  tags: [a, b]\n  wide:\n    - {long}\n    - {long}\n");
    assert_eq!(fmt(&src), want);
}

#[test]
fn a_flow_map_of_nested_values_and_a_manifest_section_stay_block() {
    let src = "metadata: { name: a }\nspec: { inner: { a: 1 } }\napiVersion: v1\nkind: K\n";
    let want = "apiVersion: v1\nkind: K\nmetadata:\n  name: a\nspec:\n  inner: { a: 1 }\n";
    assert_eq!(fmt(src), want);
}

#[test]
fn quotes_go_unless_yaml_or_a_yaml_1_1_reader_needs_them() {
    let src = "a: \"plain\"\nb: 'yes'\nc: \"012\"\nd: \"{{ name }}\"\ne: \"a: b\"\nf: \"on\"\ng: 'it''s'\n";
    let want = "a: plain\nb: 'yes'\nc: '012'\nd: '{{ name }}'\ne: 'a: b'\nf: 'on'\ng: it's\n";
    assert_eq!(fmt(src), want);
}

#[test]
fn a_string_with_a_quote_or_a_control_character_takes_double_quotes() {
    assert_eq!(
        fmt("a: \"it's: x\"\nb: \"t\\tab\"\n"),
        "a: \"it's: x\"\nb: \"t\\tab\"\n"
    );
}

#[test]
fn plain_text_a_yaml_1_1_reader_cuts_in_a_flow_collection_stays_block() {
    let src = "spec:
  ask:
    - How many?
    - Today's
  yes_no:
    a: y
";
    let want = "spec:
  ask:
    - How many?
    - Today's
  yes_no:
    a: y
";
    assert_eq!(fmt(src), want);
}

#[test]
fn sequences_are_indented_under_their_key_and_items_hold_maps() {
    let src = "rules:\n- name: a\n  kind: role\n- - x\n  - z\n";
    let want = "rules:\n  - name: a\n    kind: role\n  - [x, z]\n";
    assert_eq!(fmt(src), want);
}

#[test]
fn block_scalars_keep_their_lines_and_trailing_spaces() {
    let src = "spec:\n    mapping: |\n        root = this  \n\n          .x\n    folded: >-\n        one\n        two\n";
    let want =
        "spec:\n  mapping: |\n    root = this  \n\n      .x\n  folded: >-\n    one\n    two\n";
    assert_eq!(fmt(src), want);
}

#[test]
fn multi_line_scalars_are_written_on_one_line() {
    let src = "a: one\n  two\nb: \"x\n  y\"\n";
    assert_eq!(fmt(src), "a: one two\nb: x y\n");
}

#[test]
fn documents_keep_their_separators() {
    let src = "---\na: 1\n---\nb: 2\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn anchors_aliases_and_tags_are_refused() {
    for src in ["a: &x 1\nb: *x\n", "a: !!str 1\n"] {
        assert!(
            matches!(format(src), Err(FmtError::Unsupported { line: 1, .. })),
            "{src}"
        );
    }
}

#[test]
fn invalid_yaml_is_refused() {
    assert!(matches!(format("a: [1\n"), Err(FmtError::Parse(_))));
}

#[test]
fn what_serde_writes_is_formatted_to_the_same_layout_as_a_hand_edit() {
    let mut spec = serde_norway::Mapping::new();
    spec.insert("assigner".into(), "did:web:x".into());
    spec.insert(
        "actions".into(),
        Value::Sequence(vec!["read".into(), "write".into()]),
    );
    let mut doc = serde_norway::Mapping::new();
    doc.insert("apiVersion".into(), "joinedcontext.com/v1alpha1".into());
    doc.insert("kind".into(), "Policy".into());
    doc.insert("spec".into(), Value::Mapping(spec));
    let written = serde_norway::to_string(&doc).unwrap_or_default();
    let hand = "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nspec:\n  assigner: did:web:x\n  actions:\n    - read\n    - write\n";
    assert_eq!(fmt(&written), fmt(hand));
    assert!(fmt(hand).contains("actions: [read, write]"));
}
