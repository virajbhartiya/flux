#!/bin/bash
# End-to-end test of `flux mount` (Linux, needs /dev/fuse and fusermount3).
# Usage: BIN=target/release ci/fuse-test.sh
set -euo pipefail
B=$(cd "${BIN:-target/release}" && pwd)
T=$(mktemp -d)
cleanup() { fusermount3 -u "$T/mnt" 2>/dev/null || true; kill %1 2>/dev/null || true; }
trap cleanup EXIT
"$B/flux-server" --data-dir "$T/data" --listen 127.0.0.1:7979 --create-repo demo > "$T/server.log" 2>&1 &
for i in $(seq 1 50); do curl -sf http://127.0.0.1:7979/-/healthz > /dev/null && break; sleep 0.1; done
cd "$T" && "$B/flux" attach 127.0.0.1:7979 demo wc > /dev/null && cd wc
mkdir -p src/deep/er && echo "fn main() {}" > src/main.rs && echo "nested" > src/deep/er/x.txt
printf '#!/bin/sh\necho hi\n' > run.sh && chmod +x run.sh && ln -s src/main.rs link.rs
head -c 3000000 /dev/urandom > big.bin
"$B/flux" commit -A -m "fuse test" > /dev/null
"$B/flux" branch main > /dev/null 2>&1 || true
mkdir -p "$T/mnt"
"$B/flux" mount "$T/mnt" --rev main &
MP=$!
for i in $(seq 1 50); do mountpoint -q "$T/mnt" && break; sleep 0.1; done
mountpoint "$T/mnt"
diff -r --no-dereference "$T/wc" "$T/mnt" -x .flux && echo "contents identical"
test -x "$T/mnt/run.sh" && echo "exec bit ok"
test "$(readlink "$T/mnt/link.rs")" = "src/main.rs" && echo "symlink ok"
cmp "$T/wc/big.bin" "$T/mnt/big.bin" && echo "large file ok"
if touch "$T/mnt/new" 2>/dev/null; then echo "mount is writable"; exit 1; fi
echo "read-only ok"
kill -INT $MP; wait $MP || true
if mountpoint -q "$T/mnt"; then echo "still mounted after Ctrl-C"; exit 1; fi
echo "unmounted ok"
