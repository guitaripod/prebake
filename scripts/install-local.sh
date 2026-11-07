#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo install --path . --locked --force
expected="prebake $(cargo pkgid | sed 's/.*[#@]//')"
installed="$("$HOME/.cargo/bin/prebake" --version)"
if [ "$installed" != "$expected" ]; then
    echo "installed binary reports '$installed', expected '$expected'" >&2
    exit 1
fi
if systemctl --user is-enabled --quiet prebake.service 2>/dev/null; then
    systemctl --user restart prebake.service
fi
echo "installed: $installed ($HOME/.cargo/bin/prebake)"
