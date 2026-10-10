#!/bin/sh
# Copies the platform's User Guide from a joinedcontext-docs checkout into the guide jc-assistant
# ships (AG-118, T-3226): `User-Guide/NN-*.md` and nothing else, so no runbook (`Operations/`),
# research note or task ever reaches a `source: guide`, which may be public. STAMP holds the docs
# commit the pages were copied from; a checkout with uncommitted guide changes is refused, so the
# stamp always names the text that ships. Run it after changing the guide, and commit the result.
#
#   scripts/sync-user-guide.sh DOCS_CHECKOUT
set -eu
docs=${1:?usage: sync-user-guide.sh DOCS_CHECKOUT}
src="$docs/User-Guide"
dest="$(cd "$(dirname "$0")/.." && pwd)/crates/assistant/guide"
[ -d "$src" ] || { echo "$src is no User Guide folder" >&2; exit 1; }
if [ -n "$(git -C "$docs" status --porcelain -- User-Guide)" ]; then
  echo "$src has uncommitted changes: commit them first, so STAMP names what ships" >&2
  exit 1
fi
commit=$(git -C "$docs" rev-parse HEAD)
mkdir -p "$dest"
find "$dest" -maxdepth 1 -name '*.md' -type f -delete
count=0
for page in "$src"/[0-9][0-9]-*.md; do
  [ -f "$page" ] || continue
  cp "$page" "$dest/"
  count=$((count + 1))
done
[ "$count" -gt 0 ] || { echo "$src holds no NN-*.md page" >&2; exit 1; }
printf '%s\n' "$commit" > "$dest/STAMP"
echo "$count User Guide pages of docs $commit in $dest"
