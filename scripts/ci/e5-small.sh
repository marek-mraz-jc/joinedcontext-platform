#!/bin/sh
# Fetches the embedding model the assistant crate loads into DEST, each file checked against its
# SHA-256 (T-3053, ADR-N-040 §3.1): intfloat/multilingual-e5-small at one commit, the int8 ONNX
# export and its tokenizer. `assistant::embed` checks the same hashes again before it loads them,
# so a file swapped after this script ran is refused at start rather than embedded with.
#
#   scripts/ci/e5-small.sh DEST
set -eu
dest=${1:?usage: e5-small.sh DEST}
revision=614241f622f53c4eeff9890bdc4f31cfecc418b3
base="https://huggingface.co/intfloat/multilingual-e5-small/resolve/$revision/onnx"
mkdir -p "$dest"
while read -r sha file; do
  if [ -f "$dest/$file" ] && echo "$sha  $dest/$file" | sha256sum -c - >/dev/null 2>&1; then
    continue
  fi
  curl -fsSL --retry 3 -o "$dest/$file.part" "$base/$file"
  echo "$sha  $dest/$file.part" | sha256sum -c - >/dev/null
  mv "$dest/$file.part" "$dest/$file"
done <<'FILES'
dd476dd0c2514e9b9be83aeb3853fac0763e0bdf4a71645407587d77c48a2d88 model_qint8_avx512_vnni.onnx
0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39 tokenizer.json
bbb7c1333fc4b3e27fbc9cd5d2070aabcc1d4dfb99917c3633e772f97545a6b6 config.json
d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7 special_tokens_map.json
a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b tokenizer_config.json
FILES
echo "multilingual-e5-small ($revision) in $dest"
