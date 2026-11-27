#!/usr/bin/env bash
set -euo pipefail

baseline=$(cd "$1" && pwd)
artifact=$2
shift 2
candidate=$(pwd)
candidate_target=${CANDIDATE_TARGET_DIR:-$candidate/target}
baseline_target=${BASELINE_TARGET_DIR:-$baseline/target}
output="$candidate/qualification"
mkdir -p "$output/disabled" "$output/candidate"

build() {
  local source=$1 destination=$2 target_dir=$3 target
  shift 3
  local -a features=()
  if [[ "$source" = "$candidate" ]]; then
    features=(--features cupel-tui/codemode-preview,cupel-acp/codemode-preview)
  fi
  for target in "$@"; do
    (cd "$source" && CARGO_TARGET_DIR="$target_dir" cargo build --locked --release \
      -p cupel-tui -p cupel-acp --target "$target" ${features[@]+"${features[@]}"})
    mkdir -p "$destination/$target"
    cp "$target_dir/$target/release/"{cupel,cupel-acp} "$destination/$target/"
  done
  for binary in cupel cupel-acp; do
    if [[ $# -eq 2 ]]; then
      lipo -create "$destination/$1/$binary" "$destination/$2/$binary" -output "$destination/$binary"
      lipo "$destination/$binary" -verify_arch arm64
      lipo "$destination/$binary" -verify_arch x86_64
    else
      cp "$destination/$1/$binary" "$destination/$binary"
    fi
  done
}

build "$baseline" "$output/disabled" "$baseline_target" "$@"
build "$candidate" "$output/candidate" "$candidate_target" "$@"
python3 - "$output" "$baseline" "$candidate" "$@" <<'PY' | tee "$output/binary-sizes.json"
import json
from pathlib import Path
import subprocess
import sys

out, baseline, candidate = map(Path, sys.argv[1:4])
targets = sys.argv[4:]
rows = []
for target in targets + (["universal"] if len(targets) == 2 else []):
    for name in ("cupel", "cupel-acp"):
        path = Path(name) if target == "universal" else Path(target) / name
        before = (out / "disabled" / path).stat().st_size
        after = (out / "candidate" / path).stat().st_size
        rows.append(dict(target=target, binary=name, before=before, after=after,
                         delta_bytes=after-before, delta_percent=round(100*(after-before)/before, 2)))
revision = lambda path: subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=path, text=True).strip()
print(json.dumps(dict(baseline=revision(baseline), candidate=revision(candidate), sizes=rows), indent=2))
PY
(
  cd "$output/candidate"
  tar -czf "$artifact.tar.gz" cupel cupel-acp -C "$candidate" LICENSE README.md \
    -C "$candidate/crates/cupel-acp" examples
  shasum -a 256 "$artifact.tar.gz" > sha256sums.txt
)
python3 packaging/check-release.py "$output/candidate/$artifact.tar.gz" \
  "$output/candidate/sha256sums.txt" --codemode
