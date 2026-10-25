#!/bin/sh
# The image tag of a branch preview (T-5547, Deployment/17-branch-previews): pv-<slug>-<sha12>.
# <slug> names the preview's namespace pv-<slug> too, so it is a DNS-1123 label of at most 40
# characters: the branch without a leading preview/, lowercased, each run of other characters one
# '-', no '-' at either end. A preview never gets a bare <sha>, :main or :dev-* tag: pin-images
# and dev read those.
#
#   sh scripts/ci/preview-tag.sh <branch> <40-hex sha>    prints pv-<slug>-<sha12>
#   sh scripts/ci/preview-tag.sh --slug <branch>          prints <slug>
#   sh scripts/ci/preview-tag.sh --selftest
# A copy lives in joinedcontext-platform and joinedcontext-portal; keep the two equal.
set -eu
export LC_ALL=C

slug() {
  s=$(printf '%s' "${1#preview/}" | tr '[:upper:]' '[:lower:]' | sed -e 's/[^a-z0-9][^a-z0-9]*/-/g' -e 's/^-*//' | cut -c1-40 | sed 's/-*$//')
  [ -n "$s" ] || { echo "preview-tag: branch '$1' has no letter or digit to name a preview" >&2; return 1; }
  printf '%s\n' "$s"
}

tag() {
  printf '%s' "$2" | grep -Eqx '[0-9a-f]{40}' || { echo "preview-tag: '$2' is not a 40-hex commit" >&2; return 1; }
  s=$(slug "$1") || return 1
  printf 'pv-%s-%s\n' "$s" "$(printf '%s' "$2" | cut -c1-12)"
}

selftest() {
  sha=0123456789abcdef0123456789abcdef01234567
  long=feature/$(printf 'x%.0s' $(seq 62))
  check() {
    got=$(tag "$1" "$sha")
    [ "$got" = "$2" ] || { echo "FAIL: '$1' -> '$got', want '$2'"; exit 1; }
    s=${got#pv-}; s=${s%-*}
    [ ${#s} -le 40 ] || { echo "FAIL: '$1' slug '$s' is over 40 characters"; exit 1; }
    printf '%s' "$s" | grep -Eqx '[a-z0-9]([-a-z0-9]*[a-z0-9])?' || { echo "FAIL: '$1' slug '$s' is no DNS-1123 label"; exit 1; }
  }
  check preview/map-legend pv-map-legend-0123456789ab
  check agent/worker-t5547/T-5547 pv-agent-worker-t5547-t-5547-0123456789ab
  check Preview/UPPER_Case.dots pv-preview-upper-case-dots-0123456789ab
  check "preview/--a//b__" pv-a-b-0123456789ab
  # 70 characters cut at 40; then a cut that ends on '-' drops it
  check "$long" "pv-feature-$(printf 'x%.0s' $(seq 32))-0123456789ab"
  check "preview/$(printf 'ab-%.0s' $(seq 20))" "pv-$(printf 'ab-%.0s' $(seq 13))a-0123456789ab"
  if tag "preview/---" "$sha" 2>/dev/null; then echo "FAIL: a branch without a letter got a tag"; exit 1; fi
  if tag preview/x "$(printf '%s' "$sha" | cut -c1-12)" 2>/dev/null; then echo "FAIL: a short sha got a tag"; exit 1; fi
  if tag preview/x "$(printf '%s' "$sha" | tr a-f A-F)" 2>/dev/null; then echo "FAIL: an uppercase sha got a tag"; exit 1; fi
  echo "preview-tag selftest: ok"
}

case "${1:-}" in
  --selftest) selftest ;;
  --slug) [ $# -eq 2 ] || { echo "usage: preview-tag.sh --slug <branch>" >&2; exit 2; }; slug "$2" ;;
  *) [ $# -eq 2 ] || { echo "usage: preview-tag.sh <branch> <40-hex sha> | --slug <branch> | --selftest" >&2; exit 2; }; tag "$1" "$2" ;;
esac
