#!/bin/sh
# Fails if a cargo-build-sbf log reports a stack frame above 4 KiB. cargo-build-sbf only prints
# this and still exits 0, while the overflow corrupts locals at run time
# (rules/global_rules.md). Used by `make program`; `make program-control` proves it fires.
# Matches the current platform-tools wording and the older "Stack offset of N exceeded".
set -u
log=$1
pattern='overflows the maximum allowed frame space|Stack offset of [0-9]+ exceeded'
if grep -qE "$pattern" "$log"; then
    grep -E "$pattern" "$log"
    echo "program: a stack frame exceeds 4 KiB (see above)"
    exit 1
fi
