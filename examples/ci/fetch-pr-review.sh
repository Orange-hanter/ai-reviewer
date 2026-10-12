#!/usr/bin/env bash
# Скачивание и сверка prebuilt-бинаря pr-review по пину (RFC 16, решение 9).
#
# ШАБЛОН для репозитория-потребителя: скопируйте в ops/ai-review/fetch-pr-review.sh.
# Workflow читает ЭТОТ файл и пин ops/ai-reviewer-ref из БАЗОВОЙ ветки (git show
# origin/<base>:…), поэтому PR не может подменить ни версию движка, ни код сверки.
#
#   fetch-pr-review.sh <файл-пина> <каталог-назначения>
#
# Пин — одна строка из двух полей:   v0.5.0 sha256:<sha256 файла SHA256SUMS>
# Релиз <tag> обязан содержать:
#   SHA256SUMS                         строки `<sha256-hex>  <имя-файла>` (формат sha256sum)
#   pr-review-<target-triple>.tar.gz   в корне архива — исполняемый файл `pr-review`
# Цепочка доверия: пин → SHA256SUMS → архив. Бинарь распаковывается только после того,
# как обе сверки сошлись; до этого он не существует на диске.
#
# Результат: путь к бинарю в stdout и строка `bin=<путь>` в $GITHUB_OUTPUT (если задан).
# Пропуск — `bin=` пустой, следующий шаг workflow не запускается.
#
# Семантика выхода — две разные политики, и смешивать их нельзя:
#   exit 0, bin=   — FAIL-OPEN: инфраструктура поставки недоступна (пин пуст — bootstrap,
#                    релиз/сеть не отвечают, платформа раннера не собирается, нет sha256-
#                    утилиты). Это та же политика, что у движка: недоступность ревью не
#                    блокирует чужой merge.
#   exit 1         — FAIL-CLOSED: хэш не сошёлся или пин битый. Это не сбой ревью, а сигнал
#                    подмены артефакта (или ошибка в пине базовой ветки): тихо пропустить
#                    его значило бы, что подменённый релиз выключает гейт незаметно.
#
# Переменные (для тестов и зеркал; в CI оставить по умолчанию):
#   AI_REVIEWER_RELEASE_URL  база загрузки, по умолчанию
#                            https://github.com/Orange-hanter/ai-reviewer/releases/download
#                            (берётся <база>/<tag>/<файл>; file:// допустим)
#   AI_REVIEWER_UNAME_S/_M   подмена `uname -s` / `uname -m` (тест неизвестной платформы)
set -euo pipefail

log() { printf 'fetch-pr-review: %s\n' "$*" >&2; }

emit_bin() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    printf 'bin=%s\n' "$1" >> "$GITHUB_OUTPUT"
  fi
  if [ -n "$1" ]; then
    printf '%s\n' "$1"
  fi
}

# Fail-open: ревью пропускается, merge не блокируется.
skip() {
  log "$* — ревью пропускается (fail-open, merge не блокируется)"
  emit_bin ''
  exit 0
}

# Fail-closed: подмена артефакта или битый пин.
die() {
  log "ОШИБКА: $*"
  emit_bin ''
  exit 1
}

[ "$#" -eq 2 ] || { log "использование: fetch-pr-review.sh <файл-пина> <каталог>"; exit 2; }
pin_file=$1
dest=$2
base_url=${AI_REVIEWER_RELEASE_URL:-https://github.com/Orange-hanter/ai-reviewer/releases/download}
base_url=${base_url%/}

# --- 1. Пин -------------------------------------------------------------------------------
[ -f "$pin_file" ] || skip "пина $pin_file нет (bootstrap-PR)"
pin_line=$(tr -d '\r' < "$pin_file" | awk 'NF { print; exit }')
[ -n "$pin_line" ] || skip "пин $pin_file пуст (bootstrap-PR)"
read -r tag sums_pin extra <<< "$pin_line" || true
if [ -z "${sums_pin:-}" ]; then
  die "пин старого формата («$tag» без хэша). Этот шаблон требует «<tag> sha256:<sha256 SHA256SUMS>» (RFC 16, решение 9); старый пин работает только со старым шаблоном (чекаут + node)"
fi
[ -z "${extra:-}" ] || die "в пине лишние поля: «$pin_line»"
[[ "$tag" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "недопустимый тег в пине: «$tag»"
[[ "$sums_pin" =~ ^sha256:[0-9a-f]{64}$ ]] || die "второе поле пина должно быть sha256:<64 hex в нижнем регистре>, получено «$sums_pin»"
sums_pin=${sums_pin#sha256:}

# --- 2. Платформа и инструменты -------------------------------------------------------------
os=${AI_REVIEWER_UNAME_S:-$(uname -s)}
arch=${AI_REVIEWER_UNAME_M:-$(uname -m)}
case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) skip "архитектура $arch не собирается в релизе" ;;
esac
case "$os" in
  Linux) triple=$arch-unknown-linux-gnu ;;
  Darwin) triple=$arch-apple-darwin ;;
  *) skip "ОС $os не собирается в релизе" ;;
esac
asset=pr-review-$triple.tar.gz

if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | awk '{ print $1 }'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | awk '{ print $1 }'; }
else
  skip "нет sha256sum/shasum — сверить хэш нечем, а несверенный бинарь не запускаем"
fi
command -v curl >/dev/null 2>&1 || skip "нет curl"
command -v tar >/dev/null 2>&1 || skip "нет tar"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fetch() {
  curl -fsSL --proto '=https,file' --proto-redir '=https' --retry 3 \
    --connect-timeout 20 --max-time 300 -o "$2" "$base_url/$tag/$1"
}

# --- 3. SHA256SUMS ← пин --------------------------------------------------------------------
log "релиз $tag, платформа $triple"
fetch SHA256SUMS "$work/SHA256SUMS" || skip "SHA256SUMS релиза $tag не скачался"
sums_actual=$(sha256_of "$work/SHA256SUMS")
[ "$sums_actual" = "$sums_pin" ] \
  || die "sha256 SHA256SUMS релиза $tag = $sums_actual, а пин требует $sums_pin. Релиз подменён или пин устарел — бинарь не запускаем"

# --- 4. Архив ← SHA256SUMS ------------------------------------------------------------------
# Формат sha256sum: `<hex>  <имя>` (текстовый режим) или `<hex> *<имя>` (бинарный).
asset_sum=$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1; exit }' "$work/SHA256SUMS")
[ -n "$asset_sum" ] || skip "в SHA256SUMS релиза $tag нет $asset — платформа не собрана"
[[ "$asset_sum" =~ ^[0-9a-f]{64}$ ]] || die "строка $asset в SHA256SUMS битая: «$asset_sum»"
fetch "$asset" "$work/$asset" || skip "$asset релиза $tag не скачался"
asset_actual=$(sha256_of "$work/$asset")
[ "$asset_actual" = "$asset_sum" ] \
  || die "sha256 $asset = $asset_actual, а SHA256SUMS требует $asset_sum. Артефакт подменён — бинарь не запускаем"

# --- 5. Распаковка только сверенного -------------------------------------------------------
mkdir -p "$work/x"
tar -xzf "$work/$asset" -C "$work/x" pr-review 2>/dev/null \
  || skip "в $asset нет файла pr-review в корне — релиз собран не по контракту"
[ -f "$work/x/pr-review" ] && [ ! -L "$work/x/pr-review" ] \
  || skip "pr-review в $asset — не обычный файл"
mkdir -p "$dest"
dest=$(CDPATH='' cd -- "$dest" && pwd)
rm -f "$dest/pr-review"
mv "$work/x/pr-review" "$dest/pr-review"
chmod 0755 "$dest/pr-review"
log "OK: $dest/pr-review ($tag, $triple, sha256 сверен)"
emit_bin "$dest/pr-review"
