#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
prefix=${WORKSPACE_MGR_PREFIX:-"${HOME}/.local"}
destination="${prefix}/bin/workspace-mgr"

if [ ! -x "${script_dir}/workspace-mgr" ]; then
    echo "workspace-mgr binary is missing beside install.sh" >&2
    exit 2
fi

install -d "${prefix}/bin"
"${script_dir}/workspace-mgr" setup
install -m 0755 "${script_dir}/workspace-mgr" "${destination}"

echo "workspace-mgr installed at ${destination}"
