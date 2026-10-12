#!/bin/sh
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# shellcheck disable=SC1091
. "$here/mcp-env.sh"

mcp_drop_placeholder_env
# Бинарь: `pr-review review-mcp`; откат на node bin/pr-review-mcp.mjs, пока JS не удалён (T33).
mcp_exec review-mcp bin/pr-review-mcp.mjs "$here"
