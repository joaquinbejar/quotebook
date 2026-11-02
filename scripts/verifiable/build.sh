#!/bin/sh
# Builds quotebook-program from a committed tree inside scripts/verifiable/Dockerfile and
# prints the SHA-256 of the result (docs/PLAN-improvements.md, I2; docs/verifiable-build.md).
#
# Usage: scripts/verifiable/build.sh [<commit>]   (default HEAD)
# Output: target/verifiable/quotebook_program.so
#
# Only committed files are built (`git archive`), always at /work inside the container, so
# the result does not depend on the checkout's path or on uncommitted changes.
set -eu
commit=${1:-HEAD}
root=$(git rev-parse --show-toplevel)
cd "$root"
rev=$(git rev-parse --verify "$commit^{commit}")
image=quotebook-verifiable:agave-4.4.0-pt-1.57
docker build --platform linux/amd64 -q -t "$image" scripts/verifiable > /dev/null
mkdir -p target/verifiable
git archive --format=tar "$rev" | docker run --rm -i --platform linux/amd64 "$image" sh -c '
  set -eu
  tar -xf - -C /work
  # Checked again inside /work, where the archived rust-toolchain.toml would apply: the pins
  # must hold there, not only where the image was built.
  # Printed to stderr (stdout carries the binary) so the build log records the toolchain.
  { echo "effective toolchain in /work:"; rustc --version; cargo --version; cargo build-sbf --version; } >&2
  rustc --version | grep -qF "rustc 1.99.1" || { echo "host rustc is not 1.99.1 in /work" >&2; rustc --version >&2; exit 1; }
  cargo build-sbf --version | grep -qF "platform-tools: v1.57" || { echo "platform-tools is not v1.57" >&2; exit 1; }
  cargo build-sbf --manifest-path program/Cargo.toml --arch v3 -- --locked > /tmp/build.log 2>&1 \
    || { cat /tmp/build.log >&2; exit 1; }
  cat target/deploy/quotebook_program.so
' > target/verifiable/quotebook_program.so
echo "commit $rev"
if command -v sha256sum > /dev/null; then sum="sha256sum"; else sum="shasum -a 256"; fi
$sum target/verifiable/quotebook_program.so
