#!/bin/sh
# Unpacks the ONNX Runtime release the assistant's embedder loads into DEST, after checking its
# SHA-256 (T-3053). fastembed's `ort` is built with `load-dynamic`: the static library `ort`
# would otherwise download needs glibc 2.38, newer than Debian bookworm's, and this Microsoft
# release runs on bookworm. 1.28 is the API `ort` 2.0.0-rc.13 binds. The library lands at
# DEST/libonnxruntime.so; ORT_DYLIB_PATH names it.
#
#   scripts/ci/onnxruntime.sh DEST
set -eu
dest=${1:?usage: onnxruntime.sh DEST}
version=1.28.3
case "$(uname -m)" in
  x86_64) arch=x64; sha=db14e4863bd37893fc59729d986ab2a0d043d10b7d44da1913c4982b7e3d009c ;;
  aarch64 | arm64) arch=aarch64; sha=6c6b1ae96d7b0be9f555092857c6a4f2b0b5587a5298d732d9e52e8253038ce7 ;;
  *) echo "onnxruntime.sh: no pinned ONNX Runtime for $(uname -m)" >&2; exit 1 ;;
esac
archive=$(mktemp)
unpacked=$(mktemp -d)
trap 'rm -rf "$archive" "$unpacked"' EXIT
curl -fsSL --retry 3 -o "$archive" \
  "https://github.com/microsoft/onnxruntime/releases/download/v${version}/onnxruntime-linux-${arch}-${version}.tgz"
echo "$sha  $archive" | sha256sum -c - >/dev/null
tar xzf "$archive" -C "$unpacked"
mkdir -p "$dest"
cp "$unpacked/onnxruntime-linux-${arch}-${version}/lib/libonnxruntime.so.${version}" "$dest/libonnxruntime.so"
echo "ONNX Runtime $version ($arch) in $dest/libonnxruntime.so"
