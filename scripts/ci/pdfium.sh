#!/bin/sh
# Unpacks the one PDFium release the assistant crate links into DEST, after checking its SHA-256
# (T-3052). kreuzberg's build script reads the directory from KREUZBERG_PDFIUM_PREBUILT; without
# it the build would download whatever bblanchon/pdfium-binaries release is newest, unpinned and
# unchecked. 7568 is the release kreuzberg 4.10.4 falls back to, the one it is built against.
#
#   scripts/ci/pdfium.sh DEST
set -eu
dest=${1:?usage: pdfium.sh DEST}
version=7568
case "$(uname -m)" in
  x86_64) arch=x64; sha=e72374349280b3f9b5d5ed4b89507ede23c5b6791906a3c81ee4b55b7cb853c6 ;;
  aarch64 | arm64) arch=arm64; sha=16320590ce9f9d964fd1a13332eb828cee20484453e6b0e754fe2302bb1c3abb ;;
  *) echo "pdfium.sh: no pinned PDFium for $(uname -m)" >&2; exit 1 ;;
esac
archive=$(mktemp)
trap 'rm -f "$archive"' EXIT
curl -fsSL --retry 3 -o "$archive" \
  "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F${version}/pdfium-linux-${arch}.tgz"
echo "$sha  $archive" | sha256sum -c - >/dev/null
mkdir -p "$dest"
tar xzf "$archive" -C "$dest"
test -f "$dest/lib/libpdfium.so"
echo "PDFium chromium/$version ($arch) in $dest"
