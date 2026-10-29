#!/bin/sh
# Failing control for the lint set, run by `make lint-control` from the workspace root.
# Every `// expect: <path>` line in lint-control/src/lib.rs must get a clippy
# `disallowed_macros` error on that line naming exactly that path. Clippy runs without
# `-D warnings`, so this also fails if [workspace.lints] stops denying the lint.
set -u
src=lint-control/src/lib.rs

# Probe lines are code lines ending in `// expect: <path>`; doc comments do not match.
want=$(grep -nE '^ +[a-z].*; // expect: [a-z_:]+$' "$src" |
    sed -E 's/^([0-9]+):.*\/\/ expect: ([a-z_:]+)$/\1 \2/')
if [ -z "$want" ]; then
    echo "lint-control: no '// expect:' probes in $src"
    exit 1
fi

out=$(RUSTFLAGS='--cfg lint_control' cargo clippy -p lint-control --lib \
    --target-dir target/lint-control 2>&1)
if [ $? -eq 0 ]; then
    echo "lint-control: clippy accepted banned macros in library code"
    exit 1
fi

# "error: use of a disallowed macro `core::assert`" followed by " --> <src>:<line>:<col>".
got=$(printf '%s\n' "$out" | awk -v src="$src" '
    /^error: use of a disallowed macro `/ { m = $NF; gsub("`", "", m); next }
    m != "" && $1 == "-->" { split($2, a, ":"); if (a[1] == src) print a[2], m; m = "" }')

missing=$(printf '%s\n' "$want" | while read -r line path; do
    printf '%s\n' "$got" | grep -qxF "$line $path" || echo "  line $line: expected $path"
done)
if [ -n "$missing" ]; then
    printf '%s\n' "$out"
    echo "lint-control: clippy did not report:"
    printf '%s\n' "$missing"
    exit 1
fi
echo "lint-control: clippy rejects all $(printf '%s\n' "$want" | wc -l | tr -d ' ') probes"
