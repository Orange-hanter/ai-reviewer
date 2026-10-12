# Общие проверки лаунчеров MCP. Source из review-mcp.sh / structure-mcp.sh.
# Только stderr: stdout зарезервирован под JSON-RPC.
# Resolve корней — RFC 10. Бинарь pr-review вместо node — RFC 16 (T31): лаунчер
# предпочитает подкоманду бинаря и откатывается на node-путь, пока JS-ядро не удалено (T33).

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

# Клон пакета: JS-лаунчер (до T33) или Rust-ядро рядом с плагином (после T33 JS не будет).
mcp_engine_ok() {
  root=$1
  mcp_is_placeholder "$root" && return 1
  [ -f "$root/bin/pr-review-mcp.mjs" ] && return 0
  [ -f "$root/rust/Cargo.toml" ] && [ -f "$root/cursor-plugin/mcp.json" ]
}

# Рантайм MCP: AI_REVIEWER_MCP_RUNTIME = auto (по умолчанию) | bin | node.
#   auto — бинарь, если найден (mcp_resolve_bin), иначе node-путь;
#   bin  — только бинарь, без него fail-fast;
#   node — только node-путь (JS-ядро; исчезает в T33).
mcp_runtime() {
  runtime=${AI_REVIEWER_MCP_RUNTIME-}
  mcp_is_placeholder "$runtime" && runtime=auto
  case $runtime in
    auto|bin|node) printf '%s\n' "$runtime" ;;
    *) mcp_die "AI_REVIEWER_MCP_RUNTIME: ожидается auto|bin|node, получено $runtime" ;;
  esac
}

# Бинарь pr-review: AI_REVIEWER_BIN → сборка в клоне (release, затем debug).
# Пустой вывод — бинаря нет. Явно заданный, но неисполняемый AI_REVIEWER_BIN — fail-fast:
# молча откатиться на node значило бы запустить не то, что настроено.
mcp_resolve_bin() {
  engine=$1
  bin_override=${AI_REVIEWER_BIN-}
  if ! mcp_is_placeholder "$bin_override"; then
    [ -f "$bin_override" ] && [ -x "$bin_override" ] || mcp_die "AI_REVIEWER_BIN не исполняемый: $bin_override"
    printf '%s\n' "$bin_override"
    return 0
  fi
  [ -n "$engine" ] || return 0
  for candidate in "$engine/rust/target/release/pr-review" "$engine/rust/target/debug/pr-review"; do
    if [ -f "$candidate" ] && [ -x "$candidate" ]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
}

# Общий хвост лаунчеров: $1 — подкоманда бинаря, $2 — JS-скрипт относительно клона.
mcp_exec() {
  subcommand=$1
  js_script=$2
  launcher_dir=$3
  runtime=$(mcp_runtime)
  engine=
  if [ "$runtime" != node ] && ! mcp_is_placeholder "${AI_REVIEWER_BIN-}"; then
    # Бинарь задан явно — клон движка не обязателен (standalone-установка).
    engine=$( (mcp_resolve_engine "$launcher_dir") 2>/dev/null || true)
  else
    engine=$(mcp_resolve_engine "$launcher_dir")
  fi
  mcp_apply_consumer_root "$engine"
  bin=
  [ "$runtime" = node ] || bin=$(mcp_resolve_bin "$engine")
  if [ -n "$bin" ]; then
    exec "$bin" "$subcommand"
  fi
  [ "$runtime" = bin ] && mcp_die "AI_REVIEWER_MCP_RUNTIME=bin, но бинарь pr-review не найден: задайте AI_REVIEWER_BIN или соберите cargo build --manifest-path $engine/rust/Cargo.toml"
  script=$engine/$js_script
  [ -f "$script" ] || mcp_die "нет $script и нет бинаря pr-review — задайте AI_REVIEWER_BIN или AI_REVIEWER_ROOT на клон пакета"
  node=$(mcp_resolve_node)
  exec "$node" "$script"
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
    mcp_die "$AI_REVIEWER_ROOT не клон пакета (нет bin/pr-review-mcp.mjs и rust/Cargo.toml) — AI_REVIEWER_ROOT должен указывать на клон пакета"
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
    # Без клона (standalone-бинарь) по умолчанию ревьюим cwd.
    REVIEW_MCP_ROOT=${engine:-$(pwd)}
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
  for name in AI_REVIEWER_ROOT REVIEW_MCP_ROOT STRUCTURE_MCP_ROOT REVIEW_PROFILE OLLAMA_API_KEY AI_REVIEWER_NODE AI_REVIEWER_BIN AI_REVIEWER_MCP_RUNTIME; do
    eval "value=\${$name-}"
    if mcp_is_placeholder "$value"; then
      unset "$name"
    fi
  done
}
