#!/bin/sh
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# shellcheck disable=SC1091
. "$here/mcp-env.sh"

mcp_drop_placeholder_env
engine=$(mcp_resolve_engine "$here")
mcp_apply_consumer_root "$engine"
script=$engine/bin/pr-review-mcp.mjs
[ -f "$script" ] || mcp_die "нет $script — AI_REVIEWER_ROOT должен указывать на клон пакета"
node=$(mcp_resolve_node)
exec "$node" "$script"
