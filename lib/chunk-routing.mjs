/**
 * Маршрутизация ролей по содержимому чанка (RFC 04, решение 2).
 *
 * Состав жюри не зависел ни от чего: чанк из одних markdown-планов и JSON-фикстур получал
 * тех же трёх специалистов, что и чанк с кодом гейта, и специалисты честно отвечали «в моей
 * области замечаний нет» — за полную цену вызова. Здесь решается, заслуживает ли чанк
 * полного MoA.
 *
 * Потребитель — оркестрация `reviewChunkMoA`. Бинарь-мост engine её не делает (там
 * planChunks/demote/aggregate — чистые преобразования данных); Rust-порт —
 * rust/src/chunk_routing.rs, его потребитель — RealEngine в rust/src/engine.rs (RFC 16 T37).
 *
 * Граница проведена по признаку «исполняемое», а не по классу роутера: `config`
 * у `classifyFilePath` одинаково метит `.github/workflows/deploy.yml` и
 * `fixtures/*.json`, а это разные по цене ошибки.
 */

/** Запускается или конфигурирует запуск: правка здесь способна сломать прод или утечь секрет. */
const EXECUTABLE_CONFIG = /(?:^|\/)(?:Dockerfile|Makefile|Containerfile|Procfile)(?:$|\.)|\.(?:ya?ml|sh|bash|zsh|fish|sql|tf|tfvars|nix|gradle|cmake)$/i;

/** Всё под .github/ — workflow, actions, политики репозитория: исполняемая поверхность CI. */
const CI_PATH = /(?:^|\/)\.github\//i;

/**
 * Нужен ли чанку полный состав специалистов.
 *
 * `true` (полный MoA) — если хотя бы один файл чанка: код (`logic` по classifyFilePath),
 * исполняемая конфигурация, путь под `.github/`, либо чувствительный путь профиля.
 * `true` также когда состав чанка неизвестен (`unknownPaths` или пустой `paths`): решать
 * «кода нет» по неполным данным нельзя — это ровно тот случай, когда ошибка молчалива.
 *
 * `false` — чанк из данных и текста (markdown, JSON-схемы и fixtures, снапшоты).
 */
export function chunkNeedsSpecialists(chunk, { fileClass, sensitive = null } = {}) {
  const paths = chunk?.paths;
  if (!Array.isArray(paths) || !paths.length || chunk?.unknownPaths) return true;
  const classOf = typeof fileClass === 'function' ? fileClass : () => 'other';
  return paths.some((p) => {
    const path = String(p || '');
    if (!path) return true;
    if (classOf(path) === 'logic') return true;
    if (EXECUTABLE_CONFIG.test(path) || CI_PATH.test(path)) return true;
    return Boolean(sensitive?.test?.(path));
  });
}

/** Короткая строка для лога/observability: какие чанки урезаны и сколько вызовов сэкономлено. */
export function formatChunkRoutingLog(decisions, rolesPerChunk) {
  const lean = decisions.reduce((n, full) => n + (full ? 0 : 1), 0);
  if (!lean) return '';
  const saved = lean * Math.max(0, rolesPerChunk);
  return `chunk-routing: ${lean} из ${decisions.length} чанк(ов) без кода — только агрегатор, `
    + `−${saved} вызов(ов) специалистов`;
}
