#!/bin/sh
# Which ci.yml jobs a change needs (T-4696, as the Portal's T-4985). Prints rust=, pipelines= and
# widget= as true/false and crates= for $GITHUB_OUTPUT, from the paths changed between <base>
# and HEAD (or read on stdin, one per line). No base, a line `*`, or ci.yml itself runs
# everything; gitleaks runs on every push, decided in ci.yml.
#
#   sh scripts/ci/changes.sh "$base"
#   git diff --name-only "$base" HEAD | sh scripts/ci/changes.sh -
#   sh scripts/ci/changes.sh --selftest
#
# rust runs for every path except docs/ and the top-level *.md, so a new folder is covered by
# default; tools/model-tools/ runs it too, since jc-core and the gateway compile files from there
# (include_bytes!/include_str!); the widget's files are compiled into the assistant crate
# (include_str!), so a widget change runs both.
# crates= names the folders under crates/ the change touched, or `all` when it touched anything
# else the rust job builds from (Cargo.toml, Cargo.lock, tools/, vendor/, a new folder): the rust
# job checks those crates and every workspace crate depending on them (scripts/ci/rdeps.py,
# T-6153). Model Tools left the per-push lane for ci-full (T-6153: 146 s a push, no catch in 7 days).
set -eu

classify() {
  rust=false pipelines=false widget=false crates='' allcrates=false
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    case $f in
      '*' | .github/workflows/ci.yml) everything ;;
    esac
    case $f in
      crates/assistant/widget/*) widget=true ;;
    esac
    case $f in
      examples/ingestion/*) pipelines=true ;;
    esac
    case $f in
      docs/*) ;;
      crates/*/*) rust=true d=${f#crates/} && crates="$crates ${d%%/*}" ;;
      */*) rust=true allcrates=true ;;
      *.md) ;;
      *) rust=true allcrates=true ;;
    esac
  done
}

everything() {
  rust=true pipelines=true widget=true allcrates=true
}

report() {
  if [ "$allcrates" = true ]; then
    crates=all
  else
    crates=$(printf '%s' "$crates" | tr ' ' '\n' | sed '/^$/d' | sort -u | tr '\n' ' ')
  fi
  printf 'rust=%s\npipelines=%s\nwidget=%s\ncrates=%s\n' "$rust" "$pipelines" "$widget" "${crates% }"
}

selftest() {
  check() {
    got=$(printf '%b' "$2" | { classify; report; } | tr '\n' ' ')
    [ "$got" = "$3" ] || { echo "changes selftest: $1: got '$got', want '$3'" >&2; exit 1; }
  }
  check "one crate" 'crates/context-gateway/src/lib.rs\n' "rust=true pipelines=false widget=false crates=context-gateway "
  check "two crates, one twice" 'crates/jcctl/src/main.rs\ncrates/jc-core/src/lib.rs\ncrates/jcctl/Cargo.toml\n' "rust=true pipelines=false widget=false crates=jc-core jcctl "
  check "lock" 'Cargo.lock\n' "rust=true pipelines=false widget=false crates=all "
  check "a crate and the lock" 'crates/jcctl/src/main.rs\nCargo.lock\n' "rust=true pipelines=false widget=false crates=all "
  check "a file jc-core includes" 'tools/model-tools/models/unece-rec20.json\n' "rust=true pipelines=false widget=false crates=all "
  check "a committed model" 'examples/datamodels/x/model.linkml.yaml\n' "rust=true pipelines=false widget=false crates=all "
  check "a pipeline example" 'examples/ingestion/gtfs-rt/bento.yaml\n' "rust=true pipelines=true widget=false crates=all "
  check "the widget" 'crates/assistant/widget/widget.js\n' "rust=true pipelines=false widget=true crates=assistant "
  check "a file beside the crates" 'crates/README.md\n' "rust=true pipelines=false widget=false crates=all "
  check "docs only" 'docs/x.md\nREADME.md\nSECURITY.md\n' "rust=false pipelines=false widget=false crates= "
  check "nested md is not docs" 'crates/context-gateway/src/mcp/primer.md\n' "rust=true pipelines=false widget=false crates=context-gateway "
  check "unknown folder runs every crate" 'newdir/x.txt\n' "rust=true pipelines=false widget=false crates=all "
  check "the workflow" '.github/workflows/ci.yml\n' "rust=true pipelines=true widget=true crates=all "
  check "another workflow" '.github/workflows/image.yml\n' "rust=true pipelines=false widget=false crates=all "
  check "no base" '*\n' "rust=true pipelines=true widget=true crates=all "
  check "nothing" '' "rust=false pipelines=false widget=false crates= "
  echo "changes selftest: ok"
}

case ${1:-} in
  --selftest) selftest ;;
  -) classify; report ;;
  "") everything; report ;;
  *)
    if paths=$(git diff --name-only "$1" HEAD 2>/dev/null); then
      classify <<EOF
$paths
EOF
    else
      # A base the shallow clone does not hold is no base either.
      everything
    fi
    report
    ;;
esac
