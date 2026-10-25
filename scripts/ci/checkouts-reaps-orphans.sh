#!/usr/bin/env bash
# The checkouts image reaps orphaned processes (T-5638): its ENTRYPOINT starts with tini, and a
# process orphaned inside the container (as git's detached gc is) leaves no zombie behind.
# Usage: scripts/ci/checkouts-reaps-orphans.sh <image ref>
set -euo pipefail
image=${1:?usage: $0 <image ref>}
entrypoint=$(docker image inspect -f '{{json .Config.Entrypoint}}' "$image")
case "$entrypoint" in
  '["/usr/bin/tini","--",'*) ;;
  *) echo "the checkouts ENTRYPOINT must start with /usr/bin/tini --, got $entrypoint" >&2; exit 1 ;;
esac
# Twenty grandchildren whose parent exits at once: the kernel hands each to PID 1 (tini).
zombies=$(docker run --rm --entrypoint /usr/bin/tini "$image" -- sh -c '
  i=0; while [ $i -lt 20 ]; do sh -c "sleep 0.2 &"; i=$((i+1)); done
  sleep 2
  grep -l "^State:[[:space:]]*Z" /proc/[0-9]*/status 2>/dev/null | wc -l')
if [ "$zombies" -ne 0 ]; then
  echo "$zombies zombie processes left in the checkouts image after 20 orphans exited" >&2
  exit 1
fi
echo "checkouts image: tini is PID 1 and reaped 20 orphans"
