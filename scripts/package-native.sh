#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: package-native.sh <rust-host-target>" >&2
    exit 2
fi

expected_target="$1"
case "$expected_target" in
    x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|aarch64-apple-darwin)
        ;;
    *)
        echo "unsupported release target: $expected_target" >&2
        exit 2
        ;;
esac

actual_target="$(rustc -vV | sed -n 's/^host: //p')"
if [ "$actual_target" != "$expected_target" ]; then
    echo "runner host $actual_target does not match $expected_target" >&2
    exit 2
fi

cargo build --locked --release
target/release/workspace-mgr --version

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)"
archive_name="workspace-mgr-${version}-${expected_target}"
mkdir -p "dist/${archive_name}"
cp target/release/workspace-mgr LICENSE README.md "dist/${archive_name}/"
tar -C dist -czf "dist/${archive_name}.tar.gz" "${archive_name}"
(
    cd dist
    shasum -a 256 "${archive_name}.tar.gz" > "${archive_name}.tar.gz.sha256"
)
