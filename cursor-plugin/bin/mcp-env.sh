# Общие проверки лаунчеров MCP. Source из review-mcp.sh / structure-mcp.sh.
# Только stderr: stdout зарезервирован под JSON-RPC.
# Resolve корней — RFC 10.

mcp_die() {
  printf 'ai-reviewer MCP: %s\n' "$*" >&2
  exit 1
}

mcp_is_placeholder() {
  value=$1
  case $value in
    ''|\$\{*\}) return 0 ;;
  esac
  return 1
}

mcp_engine_ok() {
  root=$1
  mcp_is_placeholder "$root" && return 1
  [ -f "$root/bin/pr-review-mcp.mjs" ]
}

mcp_resolve_node() {
  node_override=${AI_REVIEWER_NODE-}
  if ! mcp_is_placeholder "$node_override"; then
    [ -x "$node_override" ] || mcp_die "AI_REVIEWER_NODE не исполняемый: $node_override"
    printf '%s\n' "$node_override"
    return 0
  fi
  if command -v node >/dev/null 2>&1; then
    command -v node
    return 0
  fi
  for candidate in \
    "$HOME/.local/bin/node" \
    /opt/homebrew/bin/node \
    /usr/local/bin/node
  do
    if [ -x "$candidate" ]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  mcp_die "node ≥ 22 не найден (PATH GUI Cursor часто без node). Задайте AI_REVIEWER_NODE."
}

mcp_resolve_engine() {
  launcher_dir=$1
  if mcp_engine_ok "${AI_REVIEWER_ROOT-}"; then
    printf '%s\n' "$AI_REVIEWER_ROOT"
    return 0
  fi
  if ! mcp_is_placeholder "${AI_REVIEWER_ROOT-}"; then
    mcp_die "нет $AI_REVIEWER_ROOT/bin/pr-review-mcp.mjs — AI_REVIEWER_ROOT должен указывать на клон пакета"
  fi
  engine_file=$launcher_dir/../.engine-root
  if [ -f "$engine_file" ]; then
    root=$(head -n 1 "$engine_file" | tr -d '\r')
    if mcp_engine_ok "$root"; then
      printf '%s\n' "$root"
      return 0
    fi
    mcp_die ".engine-root не указывает на клон пакета: $root. Перезапустите cursor-plugin/bin/install-local.sh."
  fi
  dir=$(pwd)
  while [ -n "$dir" ] && [ "$dir" != / ]; do
    if mcp_engine_ok "$dir"; then
      printf '%s\n' "$dir"
      return 0
    fi
    parent=$(dirname "$dir")
    [ "$parent" = "$dir" ] && break
    dir=$parent
  done
  plugin_parent=$(CDPATH= cd -- "$launcher_dir/../.." && pwd)
  if mcp_engine_ok "$plugin_parent"; then
    printf '%s\n' "$plugin_parent"
    return 0
  fi
  mcp_die "движок не найден (нет AI_REVIEWER_ROOT, нет .engine-root, cwd не клон пакета). Запустите cursor-plugin/bin/install-local.sh или задайте корень в Plugins → Configure."
}

mcp_apply_consumer_root() {
  engine=$1
  if mcp_is_placeholder "${REVIEW_MCP_ROOT-}"; then
    REVIEW_MCP_ROOT=$engine
    export REVIEW_MCP_ROOT
  fi
  [ -d "$REVIEW_MCP_ROOT" ] || mcp_die "REVIEW_MCP_ROOT не существует: $REVIEW_MCP_ROOT"
  if mcp_is_placeholder "${STRUCTURE_MCP_ROOT-}"; then
    STRUCTURE_MCP_ROOT=$REVIEW_MCP_ROOT
    export STRUCTURE_MCP_ROOT
  fi
  [ -d "$STRUCTURE_MCP_ROOT" ] || mcp_die "STRUCTURE_MCP_ROOT не существует: $STRUCTURE_MCP_ROOT"
}

mcp_drop_placeholder_env() {
  for name in AI_REVIEWER_ROOT REVIEW_MCP_ROOT STRUCTURE_MCP_ROOT REVIEW_PROFILE OLLAMA_API_KEY AI_REVIEWER_NODE; do
    eval "value=\${$name-}"
    if mcp_is_placeholder "$value"; then
      unset "$name"
    fi
  done
}
