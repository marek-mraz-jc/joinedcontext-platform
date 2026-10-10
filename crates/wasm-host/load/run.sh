#!/usr/bin/env bash
# The 10 000-App load test of jc-wasm-host (T-3345), on a throwaway rig sized as dev (compose.yaml).
# It runs on a GitHub-hosted runner (.github/workflows/wasm-host-10k.yml), never on a shared host.
#
#   RIG_DIR=<dir> crates/wasm-host/load/run.sh
#
# <dir> holds what the platform branch under test built: `jc-wasm-host`, `fleet` (the
# provisioner, crates/wasm-host/examples/fleet.rs), the SDK example's component
# `rust_wasm_server.wasm` and its `0001_notes.sql`. APPS (10000), RATE (5/s), DURATION (30m) and
# CACHED (700, the compiled components a shard keeps) override the run. Everything measured lands in <dir>: samples.csv every 15 s, k6-summary.json,
# results.txt. The rig and its volumes are removed at the end, also when the run fails.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
: "${RIG_DIR:?set RIG_DIR to the directory with jc-wasm-host, fleet, rust_wasm_server.wasm, 0001_notes.sql}"
RIG_DIR=$(cd "$RIG_DIR" && pwd)
export RIG_DIR APPS=${APPS:-10000} RATE=${RATE:-5} DURATION=${DURATION:-30m} CACHED=${CACHED:-700}
export RIG_UID=$(id -u) RIG_GID=$(id -g)
for f in jc-wasm-host fleet rust_wasm_server.wasm 0001_notes.sql; do
  [ -s "$RIG_DIR/$f" ] || { echo "missing $RIG_DIR/$f" >&2; exit 1; }
done
compose() { docker compose -f "$here/compose.yaml" "$@"; }
teardown() { compose --profile baseline --profile load down -v --remove-orphans >/dev/null 2>&1 || true; }
trap teardown EXIT
teardown   # a rig left by an aborted run would keep its old password and data

# Throwaway secrets for this run only: never printed, removed with the rig directory's files.
umask 077
head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$RIG_DIR/db.password"
chmod 644 "$RIG_DIR/db.password"   # read by the database's own uid inside its container
export STORE_SECRET=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
printf 'rig-root' > "$RIG_DIR/store.key"
printf '%s' "$STORE_SECRET" > "$RIG_DIR/store.secret"
printf '{"shard":"9","apps":[]}' > "$RIG_DIR/shard-empty.json"
dbpass=$(cat "$RIG_DIR/db.password")

compose up -d apps-db store
ready=
for _ in $(seq 1 60); do
  docker compose -f "$here/compose.yaml" exec -T apps-db /usr/lib/postgresql/16/bin/pg_isready -qh 127.0.0.1 2>/dev/null && { ready=1; break; }; sleep 1
done
[ -n "$ready" ] || { compose logs apps-db | tail -20 >&2; echo "apps-db did not come up" >&2; exit 1; }
# The rig's own reads and its one vacuum run without the database's 30 s statement timeout: over
# 10 000 schemas on one CPU a catalog-wide statement takes longer (the first runner run, T-3345).
psqlc() { docker compose -f "$here/compose.yaml" exec -T -e PGPASSWORD="$dbpass" -e PGOPTIONS="-c statement_timeout=0" apps-db /usr/lib/postgresql/16/bin/psql -qtAU postgres -h 127.0.0.1 -d apps -c "$1"; }
catalog_sql="select pg_database_size('apps'), (select sum(pg_total_relation_size(c.oid)) from pg_class c join pg_namespace n on n.oid = c.relnamespace where n.nspname = 'pg_catalog'), (select count(*) from pg_class), (select count(*) from pg_namespace)"
before=$(psqlc "$catalog_sql")

FLEET_DB_URL="postgres://postgres:$dbpass@127.0.0.1:15432/apps" FLEET_S3_ENDPOINT=http://127.0.0.1:19000 \
FLEET_S3_KEY=rig-root FLEET_S3_SECRET="$STORE_SECRET" \
  "$RIG_DIR/fleet" "$RIG_DIR/rust_wasm_server.wasm" "$RIG_DIR/0001_notes.sql" "$APPS" 2 "$RIG_DIR" 2>&1 | tee "$RIG_DIR/fleet.log"
psqlc "vacuum analyze" >/dev/null
after=$(psqlc "$catalog_sql")

# Idle: both shards with their Apps placed and nothing asked, beside one with nothing placed.
compose --profile baseline up -d shard-0 shard-1 shard-empty
sleep 90
pid() { docker inspect -f '{{.State.Pid}}' "wasm-host-10k-$1-1"; }
# A shard is one process: its resident set. The database is many sharing one buffer pool: the sum
# of their proportional sets, so shared memory counts once.
rss_mib() { awk '/^VmRSS/ {printf "%.1f", $2/1024}' "/proc/$(pid "$1")/status"; }
# The container's main process and its children (PostgreSQL's backends); the images have no `ps`.
pids() { local main; main=$(pid "$1"); echo "$main"; awk -v m="$main" '$4 == m {print $1}' /proc/[0-9]*/stat 2>/dev/null; }
# Memory of a part that runs as another uid, whose smaps are not readable from here: the private
# memory of every process, the shared memory once (PostgreSQL's buffer pool), the main process's
# file pages, read from /proc/<pid>/status.
mem_mib() {
  pids "$1" | while read -r p; do awk '/^RssAnon/ {a=$2} /^RssShmem/ {s=$2} /^RssFile/ {f=$2} END {print a, s, f}' "/proc/$p/status" 2>/dev/null; done |
    awk 'NR == 1 {f = $3} {a += $1; if ($2 > s) s = $2} END {printf "%.1f", (a + s + f) / 1024}'
}
cpu_s() { awk '{printf "%.2f", ($14 + $15) / 100}' "/proc/$(pid "$1")/stat"; }
idle_0=$(rss_mib shard-0); idle_1=$(rss_mib shard-1); idle_empty=$(rss_mib shard-empty)
compose --profile baseline stop shard-empty >/dev/null

# The run, sampled every 15 s.
sample() {
  echo "t,part,mem_mib,cpu_seconds,cached,hits,misses,host_db_conns" > "$RIG_DIR/samples.csv"
  while sleep 15; do
    t=$(date -u +%FT%TZ)
    conns=$(psqlc "select count(*) from pg_stat_activity where usename like 'wasm_host_%'" 2>/dev/null || echo -)
    for s in 0 1; do
      m=$(curl -fsS "http://127.0.0.1:1808$s/metrics" 2>/dev/null || true)
      pick() { awk -v k="$1" '$1 ~ "^"k"\\{" {print $2}' <<<"$m"; }
      echo "$t,shard-$s,$(rss_mib "shard-$s"),$(cpu_s "shard-$s"),$(pick jc_wasm_components_cached),$(pick jc_wasm_component_cache_hits_total),$(pick jc_wasm_component_cache_misses_total),$conns" >> "$RIG_DIR/samples.csv"
    done
    echo "$t,apps-db,$(mem_mib apps-db),,,,,$conns" >> "$RIG_DIR/samples.csv"
    echo "$t,store,$(mem_mib store),,,,,$conns" >> "$RIG_DIR/samples.csv"
  done
}
sample & sampler=$!
compose --profile load run --rm k6 || echo "k6 exited non-zero" >&2
kill "$sampler" 2>/dev/null || true

for s in 0 1; do compose logs --no-color "shard-$s" > "$RIG_DIR/shard-$s.log" 2>&1 || true; done
final=$(psqlc "$catalog_sql")
objects=$(docker run --rm --network wasm-host-10k_default -e AWS_ACCESS_KEY_ID=rig-root -e AWS_SECRET_ACCESS_KEY="$STORE_SECRET" \
  -e AWS_DEFAULT_REGION=us-east-1 amazon/aws-cli@sha256:e3e329e1d2894b7b4bbb0aacacd0a155262159b2e7a3b4275eb1f24046d3e06c \
  --endpoint-url http://store:9000 s3 ls --recursive s3://apps/ --summarize 2>/dev/null | awk '/Total Objects|Total Size/' | tr '\n' ' ' || echo unknown)
{
  echo "apps=$APPS rate=$RATE duration=$DURATION cached=$CACHED"
  for s in 0 1; do
    echo "shard-$s: $(docker inspect -f 'OOM-killed {{.State.OOMKilled}}, restarts {{.RestartCount}}, state {{.State.Status}}' "wasm-host-10k-shard-$s-1")"
  done
  echo "apps-db: $(docker inspect -f 'OOM-killed {{.State.OOMKilled}}, state {{.State.Status}}' wasm-host-10k-apps-db-1)"
  echo "catalog before (db_bytes, pg_catalog_bytes, pg_class_rows, schemas): $before"
  echo "catalog after provisioning: $after"
  echo "catalog after the run: $final"
  echo "idle RSS MiB (90 s after start, nothing asked): shard-0 $idle_0, shard-1 $idle_1, shard with nothing placed $idle_empty"
  echo "after the run: shard-0 RSS $(rss_mib shard-0) MiB, shard-1 RSS $(rss_mib shard-1) MiB, apps-db $(mem_mib apps-db) MiB, store $(mem_mib store) MiB"
  echo "peak during the run (MiB): $(awk -F, 'NR>1 && $3+0 > max[$2] {max[$2]=$3+0} END {for (p in max) printf "%s %s  ", p, max[p]}' "$RIG_DIR/samples.csv")"
  echo "store: $objects"
  for s in 0 1; do echo "shard-$s metrics:"; curl -fsS "http://127.0.0.1:1808$s/metrics" | grep -E '^jc_wasm_(components_cached|component_cache)'; done
} | tee "$RIG_DIR/results.txt"
