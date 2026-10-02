#!/usr/bin/env bash
# run every tests-ng scenario against a binary (default: target/release/dotdrop)
# usage: tests-ng/run-all.sh [test-name.sh ...]
cur=$(cd "$(dirname "$0")" && pwd)
export DT_BIN="${DT_BIN:-${cur}/../target/release/dotdrop}"
export DOTDROP_NOBANNER=1
export USER="${USER:-$(id -un)}"
fail=0; pass=0; failed=()
tests=("$@")
[ ${#tests[@]} -eq 0 ] && mapfile -t tests < <(cd "$cur" && ls ./*.sh | grep -v run-all.sh)
for t in "${tests[@]}"; do
  name=$(basename "$t")
  if (cd "$cur" && timeout 120 "./$name" >"/tmp/dd-test-$name.log" 2>&1); then
    pass=$((pass+1))
  else
    fail=$((fail+1)); failed+=("$name")
  fi
done
echo "passed: $pass failed: $fail"
printf '%s\n' "${failed[@]}"
[ "$fail" -eq 0 ]
