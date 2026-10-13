#!/bin/sh
# Копия плагина в ~/.cursor/plugins/local/ai-reviewer и якорь на клон движка.
# Не кладёт секреты и не прописывает путь engine в mcp.json.
# Лаунчеры — абсолютные пути внутри dest: cwd plugin MCP у Cursor — чужой
# workspace (не корень плагина); относительный ./bin/… даёт Connection closed.
set -eu

engine=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
src=$engine/cursor-plugin
# Клон пакета: JS-лаунчер (до T33) или Rust-ядро (RFC 20) — лаунчер выберет бинарь сам.
if [ ! -f "$src/mcp.json" ] || { [ ! -f "$engine/bin/pr-review-mcp.mjs" ] && [ ! -f "$engine/rust/Cargo.toml" ]; }; then
  printf 'ai-reviewer MCP: install-local.sh запускайте из клона пакета (нужны %s и %s или %s).\n' \
    "$src/mcp.json" "$engine/bin/pr-review-mcp.mjs" "$engine/rust/Cargo.toml" >&2
  exit 1
fi

dest=${1:-"$HOME/.cursor/plugins/local/ai-reviewer"}
mkdir -p "$dest"
rsync -a --delete --exclude .engine-root --exclude mcp.json "$src/" "$dest/"
printf '%s\n' "$engine" > "$dest/.engine-root"
chmod +x "$dest/bin/"*.sh

json_str() {
  printf '%s' "$1" | awk 'BEGIN { ORS="" } { gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); print }'
}

review_sh=$(json_str "$dest/bin/review-mcp.sh")
structure_sh=$(json_str "$dest/bin/structure-mcp.sh")
cat > "$dest/mcp.json" <<EOF
{
  "mcpServers": {
    "ai-reviewer": {
      "type": "stdio",
      "command": "/bin/sh",
      "args": ["$review_sh"],
      "env": {
        "AI_REVIEWER_ROOT": "\${AI_REVIEWER_ROOT}",
        "AI_REVIEWER_BIN": "\${AI_REVIEWER_BIN}",
        "REVIEW_MCP_ROOT": "\${REVIEW_MCP_ROOT}",
        "REVIEW_PROFILE": "\${REVIEW_PROFILE}",
        "OLLAMA_API_KEY": "\${OLLAMA_API_KEY}"
      }
    },
    "ai-reviewer-structure": {
      "type": "stdio",
      "command": "/bin/sh",
      "args": ["$structure_sh"],
      "env": {
        "AI_REVIEWER_ROOT": "\${AI_REVIEWER_ROOT}",
        "AI_REVIEWER_BIN": "\${AI_REVIEWER_BIN}",
        "REVIEW_MCP_ROOT": "\${REVIEW_MCP_ROOT}",
        "STRUCTURE_MCP_ROOT": "\${REVIEW_MCP_ROOT}"
      }
    }
  }
}
EOF

printf 'ai-reviewer MCP: плагин → %s\n' "$dest" >&2
printf 'ai-reviewer MCP: .engine-root → %s\n' "$engine" >&2
printf 'ai-reviewer MCP: лаунчер → %s\n' "$dest/bin/review-mcp.sh" >&2
printf 'ai-reviewer MCP: Developer: Reload Window. Configure и Authenticate не нужны.\n' >&2
