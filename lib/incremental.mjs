/**
 * Инкрементальный прогон: не ревьюить заново то, что не менялось с прошлого прогона.
 *
 * PR ревьюится по многу раз — каждый push прогоняет ВЕСЬ дифф целиком, хотя изменились
 * два файла из тридцати. На замере PR #6 три прогона подряд стоили полную цену каждый.
 *
 * Единица сравнения — не коммит, а **вход модели по файлу**: секция диффа плюс приложенный
 * контекст. Совпал хэш — модель увидела бы ровно то же самое и ответила бы то же самое,
 * значит вызов можно не делать, а находку взять из прошлого прогона. Сравнение по SHA
 * коммитов такой гарантии не даёт (перебазирование меняет SHA, не меняя входа) и требует
 * похода в GitHub API.
 *
 * Состояние лежит своим файлом, а не в футере sticky и не в артефакте экспорта:
 *   - футер ограничен размером комментария GitHub и хранит находки урезанными (без body и
 *     confidence) — перенести через него блокер, не расширив формат, нельзя, а формат футера
 *     это контракт с потребителем;
 *   - артефакт экспорта версионирован и читается снаружи; складывать в него служебное
 *     состояние прогона значит смешивать назначения.
 *
 * Чистый JS, Rust-порта нет.
 */

import { createHash } from 'node:crypto';
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { dirname } from 'node:path';

/** v2: хэш входа без NUL в исходнике; отпечаток включает пороги actionability и таймаут LLM. */
export const INCREMENTAL_STATE_VERSION = 2;

const sha256 = (s) => createHash('sha256').update(String(s ?? ''), 'utf8').digest('hex').slice(0, 32);

/**
 * Отпечаток входа модели по каждому файлу: секция диффа + приложенный контекст.
 * Оба слагаемых обязательны. Только дифф — и правка соседней функции в том же файле
 * (она приезжает контекстом) осталась бы незамеченной; только контекст — и не заметили бы
 * смену самих hunks.
 *
 * Разделитель — JSON, не NUL: NUL в исходнике заставляет git/GitHub считать файл бинарным
 * и прятать дифф (PR #18 показывал `lib/incremental.mjs` как ADDED, 0 additions).
 *
 * `cards` — карточки символов файла (RFC 14), уже приведённые `symbolCardsHashInput` к виду
 * БЕЗ номеров строк. Это осознанное отступление от «хэш = вход модели»: номер строки
 * объявления в промпте есть, но сдвиг неизменённого объявления не меняет того, что модель
 * скажет о вызывающем файле, а с номерами каждая правка выше объявления пере-ревьюила бы
 * всех вызывателей. Нет карточек у файла — ключа в хэше нет вовсе, и хэш совпадает с
 * прогоном без фичи байт в байт.
 */
export function fileInputHashes(changedPaths, sections, contentMap, cards = null) {
  const out = {};
  for (const path of changedPaths || []) {
    const diff = sections?.get?.(path) ?? '';
    const context = contentMap?.get?.(path) ?? '';
    const fileCards = cards?.get?.(path);
    out[path] = sha256(JSON.stringify(fileCards ? { diff, context, cards: fileCards } : { diff, context }));
  }
  return out;
}

/**
 * Отпечаток конфигурации, влияющей на результат. Сменились модели, роли, раскладка промпта,
 * нарезка чанков, порог маршрута или таймаут — прошлые находки получены другим движком и
 * сопоставлять их с новыми нельзя: прогон обязан быть полным.
 */
export function configFingerprint(cfg) {
  return sha256(JSON.stringify({
    version: cfg?.version ?? '',
    reviewMode: cfg?.reviewMode,
    promptLayout: cfg?.promptLayout,
    aggregator: cfg?.moa?.aggregator,
    references: cfg?.moa?.references,
    specialists: cfg?.specialists?.assignments?.map((a) => `${a.role}:${a.model}`),
    merge: cfg?.specialists?.merge,
    models: cfg?.models?.list,
    verifier: [cfg?.verifier?.enabled, cfg?.verifier?.model, cfg?.verifier?.strictBlocks,
      cfg?.verifier?.contextMargin,
      // карточки verifier'у (RFC 14, решение 4) — только при включении: выключенный флаг
      // отпечаток не меняет
      ...(cfg?.verifier?.symbolCards ? ['symbolCards'] : [])],
    gate: [cfg?.gate?.blockOn, cfg?.gate?.consensusMin, cfg?.gate?.confidenceMin, cfg?.gate?.strict],
    actionability: [cfg?.actionability?.threshold, cfg?.actionability?.verifyMin,
      cfg?.actionability?.verifyAdvisory],
    timeoutMs: cfg?.ollama?.requestTimeoutMs,
    limits: [cfg?.limits?.chunkBytes, cfg?.limits?.maxFileLines, cfg?.limits?.maxContextBytes,
      cfg?.limits?.skipAddedFileContext, cfg?.limits?.chunkRouting],
    profile: cfg?.profile?.name ?? cfg?.profile?.source ?? '',
    // Лимиты карточек (RFC 14) решают, какие карточки доедут до чанка, а в хэш файла это не
    // попадает. Ключ — только при включённой фиче: выключенная не меняет отпечаток, и
    // обновление движка не сбрасывает состояние у тех, кто её не включал.
    ...(cfg?.limits?.symbolCards ? {
      symbolCards: [cfg.limits.symbolCardsMax, cfg.limits.symbolCardsMaxChars, cfg.limits.symbolCardsBodyMaxLines],
    } : {}),
  }));
}

/** Читает состояние прошлого прогона. Любая проблема — null: инкрементальность не критична. */
export async function readIncrementalState(file) {
  if (!file) return null;
  try {
    const state = JSON.parse(await readFile(file, 'utf8'));
    return state && state.version === INCREMENTAL_STATE_VERSION ? state : null;
  } catch {
    return null;
  }
}

/** Пишет состояние. Fail-open: не смогли — следующий прогон просто будет полным. */
export async function writeIncrementalState(file, state, log = () => {}) {
  if (!file) return false;
  try {
    await mkdir(dirname(file), { recursive: true });
    await writeFile(file, `${JSON.stringify({ ...state, version: INCREMENTAL_STATE_VERSION })}\n`, 'utf8');
    return true;
  } catch (e) {
    log(`инкрементальное состояние не сохранено (следующий прогон будет полным): ${e.message}`);
    return false;
  }
}

/**
 * Решает, что ревьюить в этом прогоне.
 *
 * Возвращает `{ mode, reviewPaths, carried, reason, unchanged }`. `mode: 'full'` — состояние
 * неприменимо, ревьюим всё (`reviewPaths` — все пути). `mode: 'incremental'` — ревьюим
 * `reviewPaths`, а `carried` переносим из прошлого прогона как есть.
 *
 * Переносятся только находки по файлам, которые (а) остались в PR и (б) не изменились.
 * Находка по изменённому файлу не переносится никогда: её место в коде могло уехать, и
 * решать это должен свежий прогон, а не сравнение хэшей.
 */
export function planIncremental({
  state, hashes, changedPaths, configFp, fullEvery = 10,
} = {}) {
  const all = new Set(changedPaths || []);
  const full = (reason) => ({ mode: 'full', reviewPaths: all, carried: [], reason, unchanged: [] });

  if (!state) return full('нет состояния прошлого прогона');
  if (state.configFp !== configFp) return full('изменилась конфигурация прогона (модели/роли/лимиты)');
  if (fullEvery > 0 && Number(state.sinceFull || 0) + 1 >= fullEvery) {
    return full(`каждый ${fullEvery}-й прогон полный (защита от накопления дрейфа)`);
  }

  const prevHashes = state.hashes || {};
  // Файл, которого в прошлом прогоне не было, — новый для нас: его нужно ревьюить.
  const unchanged = [...all].filter((p) => prevHashes[p] && prevHashes[p] === hashes?.[p]);
  if (!unchanged.length) return full('изменились все файлы PR');

  const unchangedSet = new Set(unchanged);
  const reviewPaths = new Set([...all].filter((p) => !unchangedSet.has(p)));
  const carried = (state.findings || []).filter((f) => f?.file && unchangedSet.has(f.file));
  return {
    mode: 'incremental',
    reviewPaths,
    carried,
    unchanged,
    reason: `${unchanged.length} из ${all.size} файл(ов) не изменились с прошлого прогона`,
  };
}

/**
 * Метит перенесённые находки. Пометка нужна не для красоты: она говорит пайплайну, что
 * находка УЖЕ прошла жюри и verifier в прошлом прогоне (её статусы при ней), поэтому её
 * нельзя ни понижать как «без advisorClaims», ни проверять повторно.
 */
export function markCarried(findings, fromSha = '') {
  return (findings || []).map((f) => ({ ...f, carriedOver: true, ...(fromSha ? { carriedFromSha: fromSha } : {}) }));
}

/** Строка для sticky и лога: почему часть PR не ревьюилась заново. */
export function formatIncrementalNote(plan) {
  if (!plan || plan.mode !== 'incremental') return '';
  return `♻️ Инкрементальный прогон: ${plan.reason} — по ним перенесены находки прошлого прогона `
    + `(${plan.carried.length}), заново ревьюились только изменившиеся.`;
}
