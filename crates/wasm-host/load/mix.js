// The long-tail traffic of the 10 000-App load test (T-3345). App `i` is `load-<i>` on shard
// `i % shards`, as the fleet provisioner placed it.
//
//   mix   RATE requests a second for DURATION: HOT_SHARE of them to the hot 1 % of the Apps, the
//         rest spread over the rare ones; 90 % read the App's notes (one SQL query as the App),
//         10 % add one (one insert).
//   cold  then, PROBE Apps the mix never touched, one request each: every one fetches and
//         compiles its component first, so this is the cold latency with the cache already full.
//   warm  then the hot Apps again, compiled and kept: the warm latency.
//
// Latencies are k6 Trends per phase, read from the summary; nothing here is a budget, the
// numbers are the result (they go into the task and ADR-N-044).
import http from 'k6/http';
import exec from 'k6/execution';
import { Counter, Trend } from 'k6/metrics';

const SHARDS = (__ENV.SHARD_URLS || '').split(',').filter(Boolean);
const APPS = Number(__ENV.APPS || 10000);
const RATE = Number(__ENV.RATE || 30);
const DURATION = __ENV.DURATION || '30m';
const PROBE = Number(__ENV.PROBE || 500);
const HOT = Math.max(1, Math.floor(APPS / 100));
const HOT_SHARE = Number(__ENV.HOT_SHARE || 0.8);
const WARM = Number(__ENV.WARM || 2000);

if (SHARDS.length === 0) throw new Error('set SHARD_URLS to the shards, comma-separated');
if (!(APPS > HOT + PROBE)) throw new Error('APPS must exceed the hot Apps plus PROBE');

const seconds = (text) => {
  const m = /^(\d+)(s|m|h)$/.exec(text);
  if (!m) throw new Error(`DURATION is like 30m, not ${text}`);
  return Number(m[1]) * { s: 1, m: 60, h: 3600 }[m[2]];
};
const MIX_SECONDS = seconds(DURATION);

const latency = {
  hot: new Trend('mix_hot_ms', true),
  rare: new Trend('mix_rare_ms', true),
  cold: new Trend('cold_ms', true),
  warm: new Trend('warm_ms', true),
};
const outcomes = new Counter('answers');
// One counter per answer class that is not a success, so the summary says what failed.
const FAILED = ['400', '403', '404', '409', '413', '429', '500', '502', '503', '504'];
const failed = Object.fromEntries(FAILED.map((code) => [code, new Counter(`failed_${code}`)]));
const failedOther = new Counter('failed_other');

export const options = {
  summaryTrendStats: ['p(50)', 'p(90)', 'p(99)', 'max', 'count'],
  discardResponseBodies: true,
  scenarios: {
    mix: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      timeUnit: '1s',
      duration: DURATION,
      preAllocatedVUs: 50,
      maxVUs: 400,
      exec: 'mix',
    },
    cold: {
      executor: 'shared-iterations',
      vus: 4,
      iterations: PROBE,
      startTime: `${MIX_SECONDS + 10}s`,
      maxDuration: '20m',
      exec: 'cold',
    },
    warm: {
      executor: 'shared-iterations',
      vus: 4,
      iterations: WARM,
      startTime: `${MIX_SECONDS + 10}s`,
      maxDuration: '20m',
      exec: 'warm',
      // after cold: shared-iterations has no ordering, so warm waits for the probe by time
      // (PROBE requests at a few per second at worst).
    },
  },
};
// Warm runs after cold: the probe's worst case is its components compiled one after another.
options.scenarios.warm.startTime = `${MIX_SECONDS + 10 + Math.ceil(PROBE / 2) + 30}s`;

function call(app, kind, write) {
  const base = SHARDS[app % SHARDS.length];
  const url = `${base}/apps/load-${app}/api/notes`;
  const res = write
    ? http.post(url, JSON.stringify({ body: `k6 ${exec.scenario.iterationInTest}` }), {
        headers: { 'content-type': 'application/json' },
        tags: { kind },
      })
    : http.get(url, { tags: { kind } });
  latency[kind].add(res.timings.duration);
  outcomes.add(1, { kind, status: String(res.status) });
  if (res.status >= 400 || res.status === 0) (failed[String(res.status)] ?? failedOther).add(1, { kind });
}

export function mix() {
  const hot = Math.random() < HOT_SHARE;
  // Hot: [0, HOT). Rare: [HOT, APPS - PROBE); the last PROBE Apps are kept for the cold probe.
  const app = hot ? Math.floor(Math.random() * HOT) : HOT + Math.floor(Math.random() * (APPS - PROBE - HOT));
  call(app, hot ? 'hot' : 'rare', Math.random() < 0.1);
}

export function cold() {
  call(APPS - PROBE + exec.scenario.iterationInTest, 'cold', false);
}

export function warm() {
  call(exec.scenario.iterationInTest % HOT, 'warm', false);
}
