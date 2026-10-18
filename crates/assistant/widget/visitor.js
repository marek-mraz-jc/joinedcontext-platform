// The visitor token's proof (T-4653, API/05 §1.2.1), in a Web Worker so the page stays quick:
// the first decimal nonce whose SHA-256 of "{challenge}:{nonce}" starts with `difficulty` zero
// bits. Served as /d/visitor.js; `node --test` reads `solve` from it.
"use strict";

function zeroBits(bytes) {
  var bits = 0;
  for (var i = 0; i < bytes.length; i += 1) {
    if (bytes[i] !== 0) return bits + Math.clz32(bytes[i]) - 24;
    bits += 8;
  }
  return bits;
}

// Hashes in batches of 256 so `crypto.subtle` works in parallel.
async function solve(challenge, difficulty) {
  var encoder = new TextEncoder();
  for (var start = 0; ; start += 256) {
    var batch = [];
    for (var n = start; n < start + 256; n += 1) {
      batch.push(crypto.subtle.digest("SHA-256", encoder.encode(challenge + ":" + n)));
    }
    var hashes = await Promise.all(batch);
    for (var i = 0; i < hashes.length; i += 1) {
      if (zeroBits(new Uint8Array(hashes[i])) >= difficulty) return String(start + i);
    }
  }
}

if (typeof module !== "undefined") {
  module.exports = { solve: solve, zeroBits: zeroBits };
} else {
  onmessage = function (event) {
    solve(String(event.data.challenge), Number(event.data.difficulty)).then(function (nonce) { postMessage(nonce); });
  };
}
