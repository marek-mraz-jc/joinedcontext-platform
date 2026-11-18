#!/bin/sh
# Puts the ONNX Runtime library the assistant's embedder loads into DEST, after checking its
# SHA-256 (T-3053). fastembed's `ort` is built with `load-dynamic`: the static library `ort`
# would otherwise download needs glibc 2.38, newer than Debian bookworm's, and this Microsoft
# release runs on bookworm. 1.28 is the API `ort` 2.0.0-rc.13 binds. The library lands at
# DEST/libonnxruntime.so; ORT_DYLIB_PATH names it.
#
# x64 comes from our own release asset (T-7224: github.com/microsoft/onnxruntime answers 404
# since 2026-11-17): libonnxruntime.so.1.28.3 copied out of the signed image
# ghcr.io/marek-mraz-jc/jc@sha256:f0baf5e7bbc21032426003d39c047fd18f09fcd1537749b3af672240b92d2284
# (image.yml, main 799d0eab), which unpacked it from Microsoft's archive, sha256 db14e4863bd3...
# aarch64 keeps Microsoft's archive: no jc image or runner is arm64, so no copy of it exists.
#
#   scripts/ci/onnxruntime.sh DEST
set -eu
dest=${1:?usage: onnxruntime.sh DEST}
version=1.28.3
download=$(mktemp)
unpacked=$(mktemp -d)
trap 'rm -rf "$download" "$unpacked"' EXIT
mkdir -p "$dest"
case "$(uname -m)" in
  x86_64)
    arch=x64
    curl -fsSL --retry 3 -o "$download" \
      "https://github.com/marek-mraz-jc/joinedcontext/releases/download/onnxruntime-${version}/libonnxruntime-linux-x64-${version}.so"
    echo "5a1ce74e56e8c4b278d5a9e9d12b7192d9f70811084dde9616da4fa1710efc5a  $download" | sha256sum -c - >/dev/null
    cp "$download" "$dest/libonnxruntime.so"
    ;;
  aarch64 | arm64)
    arch=aarch64
    curl -fsSL --retry 3 -o "$download" \
      "https://github.com/microsoft/onnxruntime/releases/download/v${version}/onnxruntime-linux-${arch}-${version}.tgz"
    echo "6c6b1ae96d7b0be9f555092857c6a4f2b0b5587a5298d732d9e52e8253038ce7  $download" | sha256sum -c - >/dev/null
    tar xzf "$download" -C "$unpacked"
    cp "$unpacked/onnxruntime-linux-${arch}-${version}/lib/libonnxruntime.so.${version}" "$dest/libonnxruntime.so"
    ;;
  *) echo "onnxruntime.sh: no pinned ONNX Runtime for $(uname -m)" >&2; exit 1 ;;
esac
echo "ONNX Runtime $version ($arch) in $dest/libonnxruntime.so"
