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

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)"
if [ -z "$version" ]; then
    echo "could not read the package version from Cargo.toml" >&2
    exit 2
fi
if [ "${GITHUB_REF_TYPE:-}" = "tag" ] && [ "${GITHUB_REF_NAME:-}" != "v${version}" ]; then
    echo "release tag ${GITHUB_REF_NAME:-<missing>} does not match package version v${version}" >&2
    exit 2
fi

cargo build --locked --release
target/release/workspace-mgr --version

archive_name="workspace-mgr-${version}-${expected_target}"
if [ -e "dist/${archive_name}" ]; then
    rm -rf -- "dist/${archive_name}"
fi
mkdir -p "dist/${archive_name}"
cp target/release/workspace-mgr scripts/install.sh LICENSE README.md "dist/${archive_name}/"
cp -R docs "dist/${archive_name}/docs"
chmod 0755 "dist/${archive_name}/install.sh"
sh -n "dist/${archive_name}/install.sh"
"dist/${archive_name}/workspace-mgr" setup --dry-run >/dev/null
tar -C dist -czf "dist/${archive_name}.tar.gz" "${archive_name}"
(
    cd dist
    shasum -a 256 "${archive_name}.tar.gz" > "${archive_name}.tar.gz.sha256"
    shasum -a 256 -c "${archive_name}.tar.gz.sha256"
)
