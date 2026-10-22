#!/bin/sh
# Which ci.yml jobs a change needs (T-4696, as the Portal's T-4985). Prints rust=, model-tools=,
# pipelines= and widget= as true/false for $GITHUB_OUTPUT, from the paths changed between <base>
# and HEAD (or read on stdin, one per line). No base, a line `*`, or ci.yml itself runs
# everything; gitleaks runs on every push, decided in ci.yml.
#
#   sh scripts/ci/changes.sh "$base"
#   git diff --name-only "$base" HEAD | sh scripts/ci/changes.sh -
#   sh scripts/ci/changes.sh --selftest
#
# rust runs for every path except docs/, the top-level *.md and tools/model-tools/, so a new
# folder is covered by default; the widget's files are compiled into the assistant crate
# (include_str!), so a widget change runs both. model-tools runs for its own folder, the committed
# DataModels and the crates its `jcctl model diff` and jc-manifest test build from.
set -eu

classify() {
  rust=false models=false pipelines=false widget=false
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
      tools/model-tools/* | examples/datamodels/* | crates/jcctl/* | crates/jc-manifest/* | crates/jc-core/* | Cargo.toml | Cargo.lock)
        models=true ;;
    esac
    case $f in
      docs/* | tools/model-tools/*) ;;
      */*) rust=true ;;
      *.md) ;;
      *) rust=true ;;
    esac
  done
}

everything() {
  rust=true models=true pipelines=true widget=true
}

report() {
  printf 'rust=%s\nmodel-tools=%s\npipelines=%s\nwidget=%s\n' "$rust" "$models" "$pipelines" "$widget"
}

selftest() {
  check() {
    got=$(printf '%b' "$2" | { classify; report; } | tr '\n' ' ')
    [ "$got" = "$3" ] || { echo "changes selftest: $1: got '$got', want '$3'" >&2; exit 1; }
  }
  check "rust only" 'crates/context-gateway/src/lib.rs\n' "rust=true model-tools=false pipelines=false widget=false "
  check "jcctl" 'crates/jcctl/src/main.rs\n' "rust=true model-tools=true pipelines=false widget=false "
  check "lock" 'Cargo.lock\n' "rust=true model-tools=true pipelines=false widget=false "
  check "model tools only" 'tools/model-tools/src/service.py\n' "rust=false model-tools=true pipelines=false widget=false "
  check "a committed model" 'examples/datamodels/x/model.linkml.yaml\n' "rust=true model-tools=true pipelines=false widget=false "
  check "a pipeline example" 'examples/ingestion/gtfs-rt/bento.yaml\n' "rust=true model-tools=false pipelines=true widget=false "
  check "the widget" 'crates/assistant/widget/widget.js\n' "rust=true model-tools=false pipelines=false widget=true "
  check "docs only" 'docs/x.md\nREADME.md\nSECURITY.md\n' "rust=false model-tools=false pipelines=false widget=false "
  check "nested md is not docs" 'crates/context-gateway/src/mcp/primer.md\n' "rust=true model-tools=false pipelines=false widget=false "
  check "unknown folder runs rust" 'newdir/x.txt\n' "rust=true model-tools=false pipelines=false widget=false "
  check "the workflow" '.github/workflows/ci.yml\n' "rust=true model-tools=true pipelines=true widget=true "
  check "another workflow" '.github/workflows/image.yml\n' "rust=true model-tools=false pipelines=false widget=false "
  check "no base" '*\n' "rust=true model-tools=true pipelines=true widget=true "
  check "nothing" '' "rust=false model-tools=false pipelines=false widget=false "
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
