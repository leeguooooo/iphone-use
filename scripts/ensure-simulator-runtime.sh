#!/bin/bash
# Prepare one explicit Simulator runtime on a disposable CI host; never change Xcode selection.
set -euo pipefail
[ "${CI:-}" = true ] || { echo 'Runtime installation is CI-only' >&2; exit 2; }
[ "$#" = 3 ] || { echo 'usage: ensure-simulator-runtime.sh runtime-id ios-version output-dir' >&2; exit 2; }
runtime_id=$1
runtime_version=$2
output_dir=$3
mkdir -p "$output_dir"
inspect_runtime() {
  xcrun simctl list runtimes --json > "$output_dir/inventory.json" || return 1
  python3 -c 'import json,sys; rows=json.load(open(sys.argv[1]))["runtimes"]; found=[r for r in rows if r.get("identifier")==sys.argv[2]]; sys.exit(2 if not found else 0 if len(found)==1 and found[0].get("isAvailable") is True else 1)' "$output_dir/inventory.json" "$runtime_id"
}
status=0
inspect_runtime || status=$?
if [ "$status" = 2 ]; then
  xcodebuild -downloadPlatform iOS -buildVersion "$runtime_version" -exportPath "$output_dir/download"
  dmg=$(python3 -c 'import sys; from pathlib import Path; paths=list(Path(sys.argv[1]).rglob("*.dmg")); assert len(paths)==1,paths; print(paths[0])' "$output_dir/download")
  xcodebuild -importPlatform "$dmg"
elif [ "$status" != 0 ]; then
  echo 'Runtime inventory failed or the listed runtime is unavailable; no inferred reinstall' >&2
  exit "$status"
fi
inspect_runtime
