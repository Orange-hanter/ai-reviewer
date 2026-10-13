// Контрактные тесты шаблонов examples/ci/ (RFC 20, T31): потребитель подключает движок
// prebuilt-бинарём pr-review по пину «<tag> sha256:<хэш SHA256SUMS>» из базовой ветки, а не
// чекаутом репозитория и node. Вынесены из test/pr-review.eval.test.mjs: после cutover (T33)
// JS-тесты движка удаляются, а шаблоны остаются — и их контракт вместе с ними.
//
// Профиль не нужен: файл не импортирует модули движка.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { constants as fsConstants } from 'node:fs';
import { access, chmod, mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CI_DIR = resolve(REPO_ROOT, 'examples/ci');
const FETCH_SCRIPT = resolve(CI_DIR, 'fetch-pr-review.sh');

async function workflowFiles() {
  return (await readdir(CI_DIR)).filter((name) => name.endsWith('.yml')).sort();
}

const readWorkflow = (name) => readFile(resolve(CI_DIR, name), 'utf8');

const yamlIndent = (line) => line.length - line.trimStart().length;

// Контрактные ассерты по workflow цепляются за структуру (скаляр по пути, шаг → его
// shell-скрипт), а не за сырой YAML: regex вида /…\n\s+…/ ломался от безобидной смены
// отступов, хотя поведение не менялось. Полноценный yaml-парсер брать некуда — ops-тесты
// работают без npm-зависимостей, поэтому разбираем нужное подмножество по отступам.

/** Снять кавычки и хвостовой комментарий со скалярного значения YAML. */
const yamlScalarValue = (raw) => raw.replace(/\s+#.*$/, '').trim().replace(/^['"]|['"]$/g, '');

/**
 * Скаляр по пути вложенных мэппингов: `concurrency.group` → 'pr-review-feedback'.
 * Поддерживается только блочная форма записи; inline mapping (`{a: b}`) и блочные скаляры
 * (`|`, `>`) на пути не парсятся — на них хелпер падает с явным сообщением, а не возвращает
 * мусор, который выглядел бы как «ассерт зелёный».
 */
function workflowScalar(workflow, path) {
  let lines = workflow.split('\n');
  let indent = 0;
  let raw = '';
  const keys = path.split('.');
  for (const [depth, key] of keys.entries()) {
    // Сравнение строкой, а не собранным regex: ключ с `.`/`*`/`[` иначе стал бы шаблоном.
    const start = lines.findIndex((line) => {
      if (yamlIndent(line) !== indent) return false;
      const trimmed = line.trim();
      const tail = trimmed.startsWith(`${key}:`) ? trimmed.slice(key.length + 1) : null;
      return tail !== null && (tail === '' || /^\s/.test(tail));
    });
    assert.ok(start >= 0, `ключ «${key}» из «${path}» не найден в workflow`);
    raw = yamlScalarValue(lines[start].trim().slice(key.length + 1));
    const isLeaf = depth === keys.length - 1;
    assert.ok(
      isLeaf ? !raw.startsWith('{') && !/^[|>]/.test(raw) : raw === '',
      `«${key}» в «${path}» записан неподдерживаемой формой YAML (${raw || '<пусто>'}) — хелпер разбирает только блочные мэппинги со скалярными листьями`,
    );
    const rest = lines.slice(start + 1);
    const end = rest.findIndex((line) => line.trim() && yamlIndent(line) <= indent);
    lines = end === -1 ? rest : rest.slice(0, end);
    indent += 2;
  }
  return raw;
}

/** Shell-скрипт шага по его `name:` — с дедентом, без окружающего YAML. */
function workflowStepScript(workflow, stepName) {
  const lines = workflow.split('\n');
  const start = lines.findIndex((line) => {
    const match = /^- name:\s*(.+)$/.exec(line.trim());
    return match ? yamlScalarValue(match[1]) === stepName : false;
  });
  assert.ok(start >= 0, `шаг «${stepName}» не найден в workflow`);
  const stepIndent = yamlIndent(lines[start]);
  let runIndent = null;
  const script = [];
  for (const line of lines.slice(start + 1)) {
    if (line.trim() && yamlIndent(line) <= stepIndent) break; // начался следующий шаг
    if (runIndent === null) {
      if (/^\s*run:\s*\|[0-9+-]*\s*(?:#.*)?$/.test(line)) runIndent = yamlIndent(line);
      continue;
    }
    if (line.trim() && yamlIndent(line) <= runIndent) break; // run-блок кончился
    script.push(line);
  }
  assert.ok(runIndent !== null, `у шага «${stepName}» нет run-блока`);
  const body = script.filter((line) => line.trim());
  // Пустой run-блок дал бы Math.min() === Infinity и молчаливую пустую строку — то есть
  // «зелёные» ассерты по скрипту, которого нет.
  assert.ok(body.length > 0, `run-блок шага «${stepName}» пустой`);
  const dedent = Math.min(...body.map(yamlIndent));
  return script.map((line) => line.slice(dedent)).join('\n').trim();
}

test('feedback workflow: scheduled batch persists through a bot PR', async () => {
  const workflow = await readFile(
    resolve(REPO_ROOT, 'examples/ci/pr-review-feedback.yml'),
    'utf8',
  );
  const guardScript = workflowStepScript(workflow, 'Guard against duplicate feedback PR');
  const harvestScript = workflowStepScript(workflow, 'Migrate log + harvest feedback batch');
  const prScript = workflowStepScript(workflow, 'Create feedback data PR if new records');

  // Guard считает своими только фиксированную ветку и легаси-суффикс из цифр: по голому
  // префиксу чужая bot/ai-review-feedback-experiment заблокировала бы harvest навсегда.
  assert.match(guardScript, /ref === "bot\/ai-review-feedback"/);
  assert.match(guardScript, /\^bot\\\/ai-review-feedback-\\d\+\$/);
  assert.doesNotMatch(guardScript, /startsWith\("bot\/ai-review-feedback"\)/);

  assert.match(workflow, /schedule:/);
  assert.match(workflow, /github\.event_name == 'schedule'/);
  // Harvest гоняет сверенный бинарь из пина, а не код из рабочего дерева.
  assert.match(harvestScript, /"\$PR_REVIEW_BIN" feedback-batch/);
  assert.match(harvestScript, /"\$PR_REVIEW_BIN" feedback-migrate/);
  assert.equal(workflowScalar(workflow, 'permissions.actions'), 'write');
  assert.equal(workflowScalar(workflow, 'permissions.contents'), 'write');
  // Имя ветки фиксированное (без ${GITHUB_RUN_ID}): суффикс копил по ветке в день, 14 висячих.
  assert.match(prScript, /BRANCH="bot\/ai-review-feedback"/);
  assert.doesNotMatch(prScript, /bot\/ai-review-feedback-\$\{GITHUB_RUN_ID\}/);
  // Одна ветка + force-push ⇒ одна группа concurrency на все триггеры (иначе cron и
  // labeled-событие затирают друг друга), и обе стороны сравнения проходят миграцию —
  // иначе смена схемы делает каждую старую запись «новой».
  assert.equal(workflowScalar(workflow, 'concurrency.group'), 'pr-review-feedback');
  assert.equal(workflowScalar(workflow, 'concurrency.cancel-in-progress'), 'false');
  assert.match(prScript, /--file "\$RUNNER_TEMP\/head\.jsonl" --write/);

  // Сравниваем МНОЖЕСТВА записей побайтово: без `LC_ALL=C` смена локали раннера
  // переставила бы строки и `comm` снова насчитал бы «новые» на пустом месте, а без `-u`
  // дубль в логе считался бы новой записью.
  assert.match(prScript, /jq -cS \. "\$RUNNER_TEMP\/head\.jsonl" \| LC_ALL=C sort -u/);
  assert.match(prScript, /jq -cS \. "\$LOG" \| LC_ALL=C sort -u/);

  // PR открываем по НОВЫМ ЗАПИСЯМ, а не по байтовому диффу (переупаковка схемы — не повод).
  // Проверяем по смыслу скрипта, а не по форматированию: решение считается через `comm -13`,
  // а весь `git diff` и ранний выход живут ПОСЛЕ ветки «новых записей нет» — то есть diff
  // только поясняет сообщение и не решает, открывать ли PR. Условие матчим по смыслу
  // («if … $NEW … 0»), чтобы переход на [[ … ]] или (( … )) тест не ронял.
  const prLines = prScript.split('\n');
  const decision = prLines.findIndex((line) => /^NEW=.*\bcomm -13\b/.test(line.trim()));
  assert.ok(decision >= 0, 'решение об открытии PR должно считаться через comm -13');
  // Ближайший gate ПОСЛЕ подсчёта: чужая (например, вложенная) ветка с тем же условием
  // не должна подменять собой ту, что охраняет ранний выход.
  const after = prLines.slice(decision + 1);
  const gateOffset = after.findIndex((line) => /^if\b.*\bNEW\b.*(?:-eq|==?)\s*0/.test(line.trim()));
  assert.ok(gateOffset >= 0, 'ранний выход должен стоять под условием «новых записей нет»');
  const gate = decision + 1 + gateOffset;
  // Границы ветки — по вложенности `if`/`fi`, а не «всё, что ниже по тексту»: строка после
  // закрывающего `fi` формально идёт после gate, но выполняется уже при $NEW > 0.
  // Открывающим считается только shell-форма `if …; then` — иначе `if (…) {` из инлайнового
  // node-скрипта ниже наращивал бы глубину, которую нечем закрыть (в JS нет `fi`).
  let depth = 0;
  let gateEnd = -1;
  for (let index = gate; index < prLines.length; index++) {
    const trimmed = prLines[index].trim();
    if (/^if\b.*;\s*then$/.test(trimmed)) depth += 1;
    if (/^fi\b/.test(trimmed) && (depth -= 1) === 0) {
      gateEnd = index;
      break;
    }
  }
  assert.ok(gateEnd > gate, 'ветка «новых записей нет» не закрыта — разбор скрипта сломан');
  for (const [index, line] of prLines.entries()) {
    const at = `строка ${index + 1} шага create-PR`;
    const insideGate = index > gate && index < gateEnd;
    if (/\bgit diff\b/.test(line)) {
      assert.ok(insideGate, `${at}: git diff вне ветки $NEW == 0 снова сделает переупаковку схемы поводом для PR`);
    }
    if (/^exit\s+0\b/.test(line.trim())) {
      assert.ok(insideGate, `${at}: ранний exit 0 вне ветки $NEW == 0`);
    }
  }

  // gh CLI на self-hosted раннере отсутствует — PR и dispatch через REST API (node).
  assert.match(prScript, /\/actions\/workflows\/ci\.yml\/dispatches/);
  assert.match(prScript, /\/actions\/workflows\/pr-review\.yml\/dispatches/);
  assert.doesNotMatch(workflow, /\bgh (pr|workflow) /);
  assert.doesNotMatch(workflow, /HEAD:main/);
  // Обе диспатчимые workflow — на стороне потребителя (его ci.yml и pr-review.yml обязаны
  // иметь workflow_dispatch, см. docs/integration.md); здесь проверяем только шаблон.
});

// Ассерты выше проверяют ФОРМУ шага; этот тест — его ПОВЕДЕНИЕ: те же три команды, взятые
// из workflow, прогоняются на подготовленных логах. Иначе «PR только при новых записях»
// держится на regex, и подмена сравнения прошла бы мимо теста.
test('feedback workflow: подсчёт новых записей выполняется по множеству записей', async (t) => {
  if (spawnSync('jq', ['--version']).error) {
    t.skip('jq недоступен — прогон конвейера сравнения пропущен');
    return;
  }
  const workflow = await readFile(
    resolve(REPO_ROOT, 'examples/ci/pr-review-feedback.yml'),
    'utf8',
  );
  const prScript = workflowStepScript(workflow, 'Create feedback data PR if new records');
  const compare = prScript.split('\n').filter((line) => /^(?:jq -cS|NEW=)/.test(line.trim()));
  assert.equal(compare.length, 3, 'ожидались три строки сравнения (before, after, NEW) из шага create-PR');
  // Из workflow в bash уходят только эти три формы команд — тест исполняет контракт, а не
  // произвольный текст файла: любая другая строка обязана сначала попасть в этот список.
  const allowed = [
    /^jq -cS \. "\$RUNNER_TEMP\/head\.jsonl" \| LC_ALL=C sort -u > "\$RUNNER_TEMP\/before\.jsonl"$/,
    /^jq -cS \. "\$LOG" \| LC_ALL=C sort -u > "\$RUNNER_TEMP\/after\.jsonl"$/,
    /^NEW=\$\(LC_ALL=C comm -13 "\$RUNNER_TEMP\/before\.jsonl" "\$RUNNER_TEMP\/after\.jsonl" \| wc -l \| tr -d ' '\)$/,
  ];
  for (const [index, line] of compare.entries()) {
    assert.match(line.trim(), allowed[index], `строка сравнения ${index + 1} изменилась — обнови ожидаемую форму осознанно`);
  }

  const dir = await mkdtemp(join(tmpdir(), 'feedback-new-'));
  const log = join(dir, 'log.jsonl');
  const countNew = async (head, current) => {
    await writeFile(join(dir, 'head.jsonl'), head, 'utf8');
    await writeFile(log, current, 'utf8');
    const res = spawnSync('bash', ['-c', `set -euo pipefail\n${compare.join('\n')}\necho "$NEW"`], {
      env: { ...process.env, RUNNER_TEMP: dir, LOG: log },
      encoding: 'utf8',
      timeout: 30_000, // зависший jq не должен вешать весь прогон тестов
    });
    assert.equal(res.status, 0, res.stderr);
    const parsed = Number(res.stdout.trim());
    assert.ok(Number.isInteger(parsed), `конвейер вернул не число: ${JSON.stringify(res.stdout)}`);
    return parsed;
  };

  const a = '{"pr":1,"finding":{"file":"a.ts","title":"t"}}';
  const aRepacked = '{"finding":{"title":"t","file":"a.ts"},"pr":1}'; // та же запись, другой порядок ключей
  const b = '{"pr":2,"finding":{"file":"b.ts","title":"u"}}';
  assert.equal(await countNew(`${a}\n`, `${a}\n`), 0, 'без изменений новых записей нет');
  assert.equal(await countNew(`${a}\n`, `${aRepacked}\n`), 0, 'переупаковка ключей — не новая запись (та самая регрессия 2026-08-01)');
  assert.equal(await countNew(`${a}\n`, `${a}\n${b}\n`), 1, 'настоящая новая запись должна детектироваться');
  assert.equal(await countNew(`${a}\n`, `${a}\n${a}\n`), 0, 'дубль уже известной записи не считается новой');
  assert.equal(await countNew('', `${a}\n`), 1, 'bootstrap: пустой HEAD и запись в логе — одна новая');
});


test('feedback promote workflow keeps heredocs inside run block', async () => {
  const workflow = await readFile(
    resolve(REPO_ROOT, 'examples/ci/pr-review-feedback-promote.yml'),
    'utf8',
  );
  const lines = workflow.split('\n');
  let runIndent = null;
  let heredocMarker = null;

  for (const [index, line] of lines.entries()) {
    const indent = line.match(/^\s*/)?.[0].length ?? 0;
    if (runIndent != null && line.trim() && indent <= runIndent) {
      assert.equal(
        heredocMarker,
        null,
        `heredoc ${heredocMarker} escaped run block before line ${index + 1}`,
      );
      runIndent = null;
    }
    if (runIndent == null && /^\s*run:\s*\|[0-9+-]*\s*(?:#.*)?$/.test(line)) {
      runIndent = indent;
      continue;
    }
    if (runIndent == null || !line.trim()) continue;
    if (!heredocMarker) {
      const match = /<<-?\s*(?:'([^']+)'|"([^"]+)"|([A-Za-z_][A-Za-z0-9_]*))/.exec(line);
      heredocMarker = match?.[1] ?? match?.[2] ?? match?.[3] ?? null;
    } else if (line.trim() === heredocMarker) {
      heredocMarker = null;
    }
  }

  assert.equal(heredocMarker, null, `unclosed heredoc ${heredocMarker}`);
});


// ---------------------------------------------------------------------------------------------
// Поставка движка (RFC 20, решение 9): бинарь по пину вместо чекаута + node.
// ---------------------------------------------------------------------------------------------

const FETCH_STEP = 'Fetch pinned pr-review binary';
const BIN_GUARD = "steps.engine.outputs.bin != ''";
// Подкоманды бинаря, которые шаблонам разрешено звать (rust/src/cli/mod.rs).
const SUBCOMMANDS = new Set([
  'review', 'eval', 'metrics', 'feedback', 'feedback-batch', 'feedback-migrate', 'feedback-promote',
  'structure', 'structure-mcp', 'review-mcp',
]);

/** Строки без YAML/shell-комментариев: ассерты не должны срабатывать на пояснения. */
const codeLines = (text) => text.split('\n').filter((line) => !line.trim().startsWith('#'));

/**
 * Шаги единственного job'а: [{ index, name, id, uses, ifExpr, env, text }].
 * Разбор по отступам (как workflowStepScript): поле шага — строка на отступе ключей шага,
 * а не вложенная (`env:`-значения и тело `run:` глубже).
 */
function workflowSteps(workflow) {
  const lines = workflow.split('\n');
  const stepsAt = lines.findIndex((line) => line.trim() === 'steps:');
  assert.ok(stepsAt >= 0, 'в workflow нет steps:');
  const base = yamlIndent(lines[stepsAt]);
  const raw = [];
  for (const line of lines.slice(stepsAt + 1)) {
    if (line.trim() && yamlIndent(line) <= base) break;
    if (yamlIndent(line) === base + 2 && line.trim().startsWith('- ')) raw.push([]);
    if (raw.length) raw.at(-1).push(line);
  }
  return raw.map((stepLines, index) => {
    // `if:` не разкавычиваем: yamlScalarValue сняла бы хвостовую `'` у `… != ''`.
    const field = (key, { unquote = true } = {}) => {
      for (const line of stepLines) {
        const trimmed = line.trim();
        const own = (yamlIndent(line) === base + 2 && trimmed.startsWith(`- ${key}:`))
          || (yamlIndent(line) === base + 4 && trimmed.startsWith(`${key}:`));
        if (!own) continue;
        const value = trimmed.replace(/^- /, '').slice(key.length + 1);
        return unquote ? yamlScalarValue(value) : value.replace(/\s+#.*$/, '').trim();
      }
      return null;
    };
    const envAt = stepLines.findIndex((line) => yamlIndent(line) === base + 4 && line.trim() === 'env:');
    const env = {};
    if (envAt >= 0) {
      for (const line of stepLines.slice(envAt + 1)) {
        if (line.trim() && yamlIndent(line) <= base + 4) break;
        const match = /^([A-Z0-9_]+):\s*(.*)$/.exec(line.trim());
        if (match) env[match[1]] = yamlScalarValue(match[2]);
      }
    }
    return {
      index,
      name: field('name'),
      id: field('id'),
      uses: field('uses'),
      ifExpr: field('if', { unquote: false }),
      env,
      text: codeLines(stepLines.join('\n')).join('\n'),
    };
  });
}

test('шаблоны: ни одного `node .ai-reviewer-engine/` и чекаута движка', async () => {
  const files = await workflowFiles();
  assert.deepEqual(files, [
    'pr-review-feedback-promote.yml', 'pr-review-feedback.yml', 'pr-review-metrics.yml', 'pr-review.yml',
  ]);
  for (const file of files) {
    const workflow = await readWorkflow(file);
    // Критерий T31 буквально — и шире: каталога движка в рабочем дереве больше нет вовсе.
    assert.doesNotMatch(workflow, /node \.ai-reviewer-engine\//, `${file}: node .ai-reviewer-engine/`);
    assert.doesNotMatch(workflow, /\.ai-reviewer-engine/, `${file}: остался чекаут движка`);
    assert.doesNotMatch(workflow, /repository:\s*Orange-hanter\/ai-reviewer/, `${file}: чекаут репозитория движка`);
  }
});

test('шаблоны: движку не нужен ни node, ни npm — только бинарь', async () => {
  for (const file of await workflowFiles()) {
    const workflow = await readWorkflow(file);
    const code = codeLines(workflow);
    for (const line of code) {
      // node в шаблонах — только инлайновые REST-хелперы (gh CLI на self-hosted нет);
      // любой `node <файл>` был бы JS-движком.
      for (const match of line.matchAll(/\bnode\s+(\S+)/g)) {
        assert.equal(match[1], '--input-type=module', `${file}: node запускает не инлайн-хелпер: ${line.trim()}`);
      }
      // npm — только зависимости приложения потребителя под eslint-prescan, без lifecycle.
      if (/\bnpm\s+(?:ci|i|install|exec|run|x)\b|\bnpx\b/.test(line)) {
        assert.match(line, /\(cd app && npm ci --ignore-scripts\b/, `${file}: npm-шаг вне app/: ${line.trim()}`);
      }
    }
    const calls = [...workflow.matchAll(/"\$PR_REVIEW_BIN"\s+([a-z-]+)/g)].map((match) => match[1]);
    assert.ok(calls.length > 0, `${file}: движок не вызывается через $PR_REVIEW_BIN`);
    for (const sub of calls) assert.ok(SUBCOMMANDS.has(sub), `${file}: неизвестная подкоманда ${sub}`);
  }
});

test('шаблоны: бинарь запускается только после сверки sha256 и только если она прошла', async () => {
  for (const file of await workflowFiles()) {
    const steps = workflowSteps(await readWorkflow(file));
    const fetch = steps.find((step) => step.name === FETCH_STEP);
    assert.ok(fetch, `${file}: нет шага «${FETCH_STEP}»`);
    assert.equal(fetch.id, 'engine', `${file}: id шага сверки — engine (на него смотрят if: шагов запуска)`);
    assert.match(fetch.text, /\bbash "?[^\s"]*fetch-pr-review\.sh"? /, `${file}: сверка идёт не через fetch-pr-review.sh`);
    // Скачивания мимо скрипта сверки нет: ни curl/wget релиза, ни tar в обход хэша.
    for (const step of steps) {
      if (step === fetch) continue;
      assert.doesNotMatch(step.text, /releases\/download|fetch-pr-review/, `${file}: шаг «${step.name}» качает движок мимо сверки`);
    }
    const runners = steps.filter((step) => /\$PR_REVIEW_BIN|PR_REVIEW_BIN:/.test(step.text));
    assert.ok(runners.length > 0, `${file}: нет шага запуска бинаря`);
    for (const step of runners) {
      assert.ok(step.index > fetch.index, `${file}: «${step.name}» стоит до сверки`);
      assert.ok(step.ifExpr?.includes(BIN_GUARD), `${file}: «${step.name}» без if: ${BIN_GUARD} — запустится и при пропуске`);
      assert.equal(step.env.PR_REVIEW_BIN, '${{ steps.engine.outputs.bin }}', `${file}: «${step.name}» берёт бинарь не из шага сверки`);
    }
  }
});

test('pr-review.yml: пин и скрипт сверки читаются из базовой ветки, а не из чекаута PR', async () => {
  const workflow = await readWorkflow('pr-review.yml');
  const fetch = workflowSteps(workflow).find((step) => step.name === FETCH_STEP);
  const script = workflowStepScript(workflow, FETCH_STEP);
  assert.equal(fetch.env.BASE_REF, '${{ github.base_ref }}');
  assert.match(script, /git show "origin\/\$BASE_REF:ops\/ai-reviewer-ref"/);
  assert.match(script, /git show "origin\/\$BASE_REF:ops\/ai-review\/fetch-pr-review\.sh" > "\$ENGINE\/fetch-pr-review\.sh"/);
  assert.match(script, /bash "\$ENGINE\/fetch-pr-review\.sh" "\$ENGINE\/ref" "\$ENGINE\/bin"/);
  // Рабочее дерево здесь — код PR: скрипт или пин оттуда дали бы PR выбрать движок.
  assert.doesNotMatch(script, /\bbash ops\/|<\s*ops\/ai-reviewer-ref|cat ops\//);
  // Бинарь — в $RUNNER_TEMP, вне дерева PR (на self-hosted раннере дерево переживает job).
  assert.match(script, /ENGINE="\$RUNNER_TEMP\/ai-reviewer"/);
  // Workflow запускается на pull_request (не pull_request_target): чекаут — merge-ref PR.
  assert.match(workflow, /^\s{2}pull_request:/m);
  assert.doesNotMatch(workflow, /pull_request_target/);
});

test('служебные шаблоны: пин из рабочего дерева базовой ветки, PR не чекаутится', async () => {
  for (const file of ['pr-review-feedback.yml', 'pr-review-metrics.yml', 'pr-review-feedback-promote.yml']) {
    const workflow = await readWorkflow(file);
    const steps = workflowSteps(workflow);
    const checkout = steps.find((step) => step.uses?.startsWith('actions/checkout@'));
    assert.ok(checkout, `${file}: нет чекаута`);
    if (/^\s{2}pull_request(?:_target)?:/m.test(workflow)) {
      // Событие PR (labeled) не должно приводить к чекауту кода PR: дерево — базовая ветка.
      assert.match(checkout.text, /^\s*ref: main$/m, `${file}: триггер PR, а чекаут не базовой ветки`);
    }
    assert.doesNotMatch(workflow, /github\.event\.pull_request\.head\.(?:sha|ref)/, `${file}: чекаут head PR`);
    const script = workflowStepScript(workflow, FETCH_STEP);
    assert.match(script, /^bash ops\/ai-review\/fetch-pr-review\.sh ops\/ai-reviewer-ref "\$ENGINE\/bin"$/m);
  }
});

test('секреты: не на уровне job/workflow, не у чекаутов и не у шага сверки', async () => {
  for (const file of await workflowFiles()) {
    const workflow = await readWorkflow(file);
    const head = workflow.slice(0, workflow.indexOf('    steps:'));
    assert.doesNotMatch(codeLines(head).join('\n'), /secrets\./, `${file}: секрет вне шага — достанется всем шагам`);
    for (const step of workflowSteps(workflow)) {
      if (step.uses?.startsWith('actions/checkout@') || step.name === FETCH_STEP) {
        assert.doesNotMatch(step.text, /secrets\./, `${file}: секрет у шага «${step.name ?? step.uses}»`);
      }
    }
  }
  // В ревью рабочее дерево — код PR: секреты получает только шаг запуска сверенного бинаря.
  const review = workflowSteps(await readWorkflow('pr-review.yml'));
  const withSecrets = review.filter((step) => /secrets\./.test(step.text)).map((step) => step.name);
  assert.deepEqual(withSecrets, ['Run AI review']);
  const run = review.find((step) => step.name === 'Run AI review');
  assert.match(run.text, /"\$PR_REVIEW_BIN" review/);
});

test('fetch-pr-review.sh: исполняемый, синтаксис bash валиден', async () => {
  await access(FETCH_SCRIPT, fsConstants.X_OK);
  const res = spawnSync('bash', ['-n', FETCH_SCRIPT], { encoding: 'utf8' });
  assert.equal(res.status, 0, res.stderr);
});

// ---------------------------------------------------------------------------------------------
// Поведение fetch-pr-review.sh на фейковом релизе (file://, без сети).
// ---------------------------------------------------------------------------------------------

const sha256 = (data) => createHash('sha256').update(data).digest('hex');
const TAG = 'v9.9.9';
const LINUX_X64 = { AI_REVIEWER_UNAME_S: 'Linux', AI_REVIEWER_UNAME_M: 'x86_64' };

function fetchToolsMissing() {
  for (const tool of ['bash', 'curl', 'tar']) {
    if (spawnSync(tool, ['--version']).error) return tool;
  }
  const hasSha = !spawnSync('sha256sum', ['--version']).error || !spawnSync('shasum', ['--version']).error;
  return hasSha ? null : 'sha256sum/shasum';
}

/**
 * Релиз в каталоге: <root>/<tag>/{SHA256SUMS, pr-review-<triple>.tar.gz}.
 * Порча задаётся опциями — каждая моделирует конкретный сбой поставки.
 */
async function makeRelease({
  triples = ['x86_64-unknown-linux-gnu', 'aarch64-apple-darwin'],
  sumsTriples = triples,
  binaryMode = false,
  tamperAsset = null,
  noRelease = false,
  dropAsset = null,
  member = 'pr-review',
} = {}) {
  const root = await mkdtemp(join(tmpdir(), 'fake-release-'));
  const relDir = join(root, TAG);
  const src = await mkdtemp(join(tmpdir(), 'fake-bin-'));
  await mkdir(relDir, { recursive: true });
  const sums = [];
  for (const triple of triples) {
    await writeFile(join(src, 'pr-review'), `#!/bin/sh\necho "fake-pr-review ${triple} $*"\n`);
    await chmod(join(src, 'pr-review'), 0o755);
    const asset = `pr-review-${triple}.tar.gz`;
    // member: '.' — упаковка каталогом (члены `./pr-review`), 'other' — архив без pr-review.
    const tarArgs = member === 'other'
      ? ['-czf', join(relDir, asset), '-C', join(src, '..'), basename(src)]
      : ['-czf', join(relDir, asset), '-C', src, member];
    const tar = spawnSync('tar', tarArgs, { encoding: 'utf8' });
    assert.equal(tar.status, 0, tar.stderr);
    if (sumsTriples.includes(triple)) {
      sums.push(`${sha256(await readFile(join(relDir, asset)))}${binaryMode ? ' *' : '  '}${asset}`);
    }
  }
  const sumsText = `${sums.join('\n')}\n`;
  await writeFile(join(relDir, 'SHA256SUMS'), sumsText);
  if (tamperAsset) {
    // Архив подменён ПОСЛЕ того, как SHA256SUMS посчитан (и закреплён пином).
    await writeFile(join(relDir, `pr-review-${tamperAsset}.tar.gz`), 'evil');
  }
  if (dropAsset) await rm(join(relDir, `pr-review-${dropAsset}.tar.gz`));
  if (noRelease) await rm(relDir, { recursive: true, force: true });
  return { root, pin: `${TAG} sha256:${sha256(sumsText)}` };
}

async function runFetch({ root, pin, env = LINUX_X64, pinFile = true, url = null }) {
  const dir = await mkdtemp(join(tmpdir(), 'fetch-run-'));
  const pinPath = join(dir, 'ai-reviewer-ref');
  if (pinFile) await writeFile(pinPath, `${pin}\n`);
  const out = join(dir, 'github-output');
  await writeFile(out, '');
  const dest = join(dir, 'bin');
  const res = spawnSync('bash', [FETCH_SCRIPT, pinPath, dest], {
    encoding: 'utf8',
    timeout: 60_000,
    env: {
      ...process.env,
      ...env,
      AI_REVIEWER_RELEASE_URL: url ?? pathToFileURL(root).href,
      GITHUB_OUTPUT: out,
    },
  });
  const outputs = (await readFile(out, 'utf8')).split('\n').filter(Boolean);
  const binary = join(dest, 'pr-review');
  const exists = await stat(binary).then(() => true, () => false);
  return { ...res, outputs, binary, exists };
}

test('fetch-pr-review.sh: сверенный бинарь под платформу раннера', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  const release = await makeRelease();

  const linux = await runFetch(release);
  assert.equal(linux.status, 0, linux.stderr);
  assert.deepEqual(linux.outputs, [`bin=${linux.binary}`]);
  assert.equal(linux.stdout.trim(), linux.binary);
  await access(linux.binary, fsConstants.X_OK);
  const ran = spawnSync(linux.binary, ['review'], { encoding: 'utf8' });
  assert.equal(ran.stdout.trim(), 'fake-pr-review x86_64-unknown-linux-gnu review');

  // arm64 (как его называет macOS) → aarch64-apple-darwin.
  const mac = await runFetch({ ...release, env: { AI_REVIEWER_UNAME_S: 'Darwin', AI_REVIEWER_UNAME_M: 'arm64' } });
  assert.equal(mac.status, 0, mac.stderr);
  const macRan = spawnSync(mac.binary, [], { encoding: 'utf8' });
  assert.equal(macRan.stdout.trim(), 'fake-pr-review aarch64-apple-darwin');
});

test('fetch-pr-review.sh: SHA256SUMS в бинарном режиме sha256sum (` *имя`) принимается', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  const res = await runFetch(await makeRelease({ binaryMode: true }));
  assert.equal(res.status, 0, res.stderr);
  assert.ok(res.exists);
});

test('fetch-pr-review.sh: SHA256SUMS не совпал с пином → exit 1, бинаря нет (fail-closed)', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  const release = await makeRelease();
  const res = await runFetch({ ...release, pin: `${TAG} sha256:${sha256('другой релиз')}` });
  assert.equal(res.status, 1, res.stderr);
  assert.match(res.stderr, /sha256 SHA256SUMS/);
  assert.deepEqual(res.outputs, ['bin=']);
  assert.equal(res.exists, false, 'бинарь не должен появиться до/без сверки');
});

test('fetch-pr-review.sh: архив не совпал с SHA256SUMS → exit 1, бинаря нет (fail-closed)', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  const res = await runFetch(await makeRelease({ tamperAsset: 'x86_64-unknown-linux-gnu' }));
  assert.equal(res.status, 1, res.stderr);
  assert.match(res.stderr, /подменён/);
  assert.deepEqual(res.outputs, ['bin=']);
  assert.equal(res.exists, false);
});

test('fetch-pr-review.sh: неизвестная платформа → пропуск без блокировки (fail-open)', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  const release = await makeRelease();
  for (const env of [
    { AI_REVIEWER_UNAME_S: 'Linux', AI_REVIEWER_UNAME_M: 'riscv64' },
    { AI_REVIEWER_UNAME_S: 'FreeBSD', AI_REVIEWER_UNAME_M: 'x86_64' },
  ]) {
    const res = await runFetch({ ...release, env });
    assert.equal(res.status, 0, res.stderr);
    assert.deepEqual(res.outputs, ['bin=']);
    assert.match(res.stderr, /не собирается в релизе/);
    assert.equal(res.exists, false);
  }
  // Платформа известна, но в этом релизе её сборки нет (SHA256SUMS о ней молчит).
  const partial = await runFetch(await makeRelease({ sumsTriples: ['aarch64-apple-darwin'] }));
  assert.equal(partial.status, 0, partial.stderr);
  assert.deepEqual(partial.outputs, ['bin=']);
  assert.match(partial.stderr, /платформа не собрана/);
});

test('fetch-pr-review.sh: сервер релизов не отвечает или пин пуст → пропуск (fail-open)', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  const release = await makeRelease();
  // Нет HTTP-ответа вовсе (connection refused на loopback) — аутедж, а не ответ «нет файла».
  const down = await runFetch({ ...release, url: 'https://127.0.0.1:1/releases/download' });
  assert.equal(down.status, 0, down.stderr);
  assert.deepEqual(down.outputs, ['bin=']);
  assert.match(down.stderr, /не скачался/);
  assert.equal(down.exists, false);

  const empty = await runFetch({ ...release, pin: '' });
  assert.equal(empty.status, 0, empty.stderr);
  assert.deepEqual(empty.outputs, ['bin=']);
  assert.match(empty.stderr, /bootstrap/);
  const absent = await runFetch({ ...release, pinFile: false });
  assert.equal(absent.status, 0, absent.stderr);
  assert.deepEqual(absent.outputs, ['bin=']);
});

test('fetch-pr-review.sh: релиза по тегу пина нет (404) → exit 1, а не тихий пропуск', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  // Опечатка в теге, удалённый или ещё не опубликованный релиз: сервер ответил, и ответ
  // противоречит пину. Пропуск здесь выключал бы гейт у всех потребителей без сигнала.
  const gone = await runFetch(await makeRelease({ noRelease: true }));
  assert.equal(gone.status, 1, gone.stderr);
  assert.deepEqual(gone.outputs, ['bin=']);
  assert.match(gone.stderr, /нет v9\.9\.9\/SHA256SUMS/);
  // SHA256SUMS сверен и называет архив, а архива в релизе нет.
  const partial = await runFetch(await makeRelease({ dropAsset: 'x86_64-unknown-linux-gnu' }));
  assert.equal(partial.status, 1, partial.stderr);
  assert.match(partial.stderr, /релиз неполон/);
  assert.equal(partial.exists, false);
});

test('fetch-pr-review.sh: член архива ./pr-review принимается, архив без pr-review → exit 1', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  // `tar -czf … -C dir .` — привычная упаковка; GNU tar по имени `pr-review` такой член не найдёт.
  const dot = await runFetch(await makeRelease({ member: '.' }));
  assert.equal(dot.status, 0, dot.stderr);
  assert.deepEqual(dot.outputs, [`bin=${dot.binary}`]);
  assert.equal(spawnSync(dot.binary, ['review'], { encoding: 'utf8' }).stdout.trim(),
    'fake-pr-review x86_64-unknown-linux-gnu review');
  // Сверенный архив не по контракту — ошибка сборки релиза, не аутедж.
  const other = await runFetch(await makeRelease({ member: 'other' }));
  assert.equal(other.status, 1, other.stderr);
  assert.match(other.stderr, /нет файла pr-review в корне/);
  assert.deepEqual(other.outputs, ['bin=']);
  assert.equal(other.exists, false);
});

test('fetch-pr-review.sh: пин-заглушка kit (хэш из нулей) → exit 1 ещё до сети', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  const kitPin = resolve(REPO_ROOT, 'examples/consumers/orakul/ai-reviewer-ref');
  const pin = (await readFile(kitPin, 'utf8')).trim();
  // Релиз, который совпал бы по тегу, есть — заглушку отвергает сама проверка пина.
  const release = await makeRelease();
  const res = await runFetch({ ...release, pin: pin.replace(/^\S+/, TAG) });
  assert.equal(res.status, 1, res.stderr);
  assert.match(res.stderr, /заглушка/);
  assert.deepEqual(res.outputs, ['bin=']);
  // И с тегом kit как есть, на пустом «зеркале» — тоже красный, а не пропуск.
  const asIs = await runFetch({ root: await mkdtemp(join(tmpdir(), 'empty-rel-')), pin });
  assert.equal(asIs.status, 1, asIs.stderr);
});

test('pr-review.yml: bootstrap — пропуск только без пина нового формата', async (t) => {
  if (spawnSync('git', ['--version']).error) return t.skip('git недоступен');
  const script = workflowStepScript(await readWorkflow('pr-review.yml'), FETCH_STEP);
  // Базовая ветка без скрипта сверки; пин — разный. git show читает origin/main.
  const runStep = async (pin) => {
    const repo = await mkdtemp(join(tmpdir(), 'bootstrap-'));
    const git = (...args) => {
      const res = spawnSync('git', ['-C', repo, '-c', 'user.name=t', '-c', 'user.email=t@t', ...args], { encoding: 'utf8' });
      assert.equal(res.status, 0, res.stderr);
    };
    git('init', '-q');
    await mkdir(join(repo, 'ops'), { recursive: true });
    if (pin !== null) await writeFile(join(repo, 'ops/ai-reviewer-ref'), `${pin}\n`);
    await writeFile(join(repo, 'README'), 'x\n');
    git('add', '-A');
    git('commit', '-qm', 'base');
    git('update-ref', 'refs/remotes/origin/main', 'HEAD');
    const runnerTemp = await mkdtemp(join(tmpdir(), 'runner-temp-'));
    const out = join(runnerTemp, 'github-output');
    await writeFile(out, '');
    const res = spawnSync('bash', ['-c', `set -eo pipefail\n${script}`], {
      cwd: repo,
      encoding: 'utf8',
      env: { ...process.env, BASE_REF: 'main', RUNNER_TEMP: runnerTemp, GITHUB_OUTPUT: out },
    });
    return { ...res, outputs: (await readFile(out, 'utf8')).split('\n').filter(Boolean) };
  };
  for (const pin of [null, '', 'v0.4.0']) {
    const res = await runStep(pin);
    assert.equal(res.status, 0, `пин ${JSON.stringify(pin)}: ${res.stderr}`);
    assert.deepEqual(res.outputs, ['bin=']);
  }
  // Пин нового формата есть, скрипта нет — шаг подключения пропущен: красный, а не вечный пропуск.
  const broken = await runStep(`v9.9.9 sha256:${'a'.repeat(64)}`);
  assert.equal(broken.status, 1, broken.stdout);
  assert.match(broken.stdout, /::error::.*fetch-pr-review\.sh/);
});

test('граница доверия: шаблон и docs не обещают, что код PR не исполняется с секретами', async () => {
  // prescan запускает eslint из node_modules PR с его flat-config, GRAPHIFY_BIN — из дерева PR;
  // дочерние процессы наследуют env шага с секретами. Обещание обратного — ложный инвариант.
  const workflow = await readWorkflow('pr-review.yml');
  const run = workflowSteps(workflow).find((step) => step.name === 'Run AI review');
  assert.equal(run.env.REVIEW_PRESCAN, '1');
  assert.doesNotMatch(workflow, /не исполняется ни здесь, ни с секретами/);
  assert.match(workflow, /ОСТАТОЧНЫЙ РИСК:.*REVIEW_PRESCAN=1/);
  const docs = await readFile(resolve(REPO_ROOT, 'docs/integration.md'), 'utf8');
  assert.doesNotMatch(docs, /вредоносный PR не выполнит свой код с секретами/);
  assert.match(docs, /^### Секреты и код PR$/m);
});

test('fetch-pr-review.sh: пин старого формата или битый → exit 1', async (t) => {
  const missing = fetchToolsMissing();
  if (missing) return t.skip(`${missing} недоступен`);
  const release = await makeRelease();
  const old = await runFetch({ ...release, pin: TAG });
  assert.equal(old.status, 1);
  assert.match(old.stderr, /старого формата/);
  for (const pin of [
    `${TAG} sha256:${'A'.repeat(64)}`, // верхний регистр — не sha256sum-формат
    `${TAG} ${'0'.repeat(64)}`, // без префикса sha256:
    `../evil sha256:${'0'.repeat(64)}`,
    `${TAG} sha256:${'0'.repeat(64)} extra`,
  ]) {
    const res = await runFetch({ ...release, pin });
    assert.equal(res.status, 1, `пин «${pin}» должен отвергаться: ${res.stderr}`);
    assert.equal(res.exists, false);
  }
});

test('kit Orakul: пин нового формата «<tag> sha256:<64 hex>»', async () => {
  const pin = (await readFile(resolve(REPO_ROOT, 'examples/consumers/orakul/ai-reviewer-ref'), 'utf8')).trim();
  assert.match(pin, /^v\d+\.\d+\.\d+ sha256:[0-9a-f]{64}$/);
});

// ---------------------------------------------------------------------------
// release.yml — производитель контракта T32 (RFC 20, решение 9)
// ---------------------------------------------------------------------------

const RELEASE_YML = resolve(REPO_ROOT, '.github/workflows/release.yml');
const RELEASE_TRIPLES = [
  'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-apple-darwin', 'aarch64-apple-darwin',
];

test('release.yml: собирает ровно четыре triple контракта, член архива — pr-review в корне', async () => {
  const yml = await readFile(RELEASE_YML, 'utf8');
  const triples = [...yml.matchAll(/^\s*- target:\s*(\S+)\s*$/gm)].map((m) => m[1]).sort();
  assert.deepEqual(triples, [...RELEASE_TRIPLES].sort());
  assert.match(yml, /tar -czf "dist\/pr-review-\$TARGET\.tar\.gz" -C "rust\/target\/\$TARGET\/release" pr-review/);
  assert.match(yml, /sha256sum pr-review-\*\.tar\.gz > SHA256SUMS/);
  assert.match(yml, /cargo build --release --locked /, 'релиз собирается из закоммиченного Cargo.lock');
});

test('release.yml: пишет в релиз только по тегу и только черновиком; по умолчанию права read', async () => {
  const yml = await readFile(RELEASE_YML, 'utf8');
  assert.match(yml, /^permissions:\n  contents: read$/m);
  const writes = [...yml.matchAll(/contents: write/g)];
  assert.equal(writes.length, 1, 'contents: write — только у job черновика');
  const draftJob = yml.slice(yml.indexOf('  draft-release:'));
  assert.match(draftJob, /if: github\.ref_type == 'tag'/);
  assert.match(draftJob, /gh release create "\$TAG" [^\n]*--draft/);
  // Выражения ${{ }} не подставляются в run-текст напрямую — только через env.
  for (const run of yml.matchAll(/^\s+run: \|\n((?:\s{10,}.*\n?)+)/gm)) {
    assert.doesNotMatch(run[1], /\$\{\{/, run[1]);
  }
});

test('release.yml ↔ fetch-pr-review.sh: каждый triple, который качает потребитель, собирается', async () => {
  const script = await readFile(FETCH_SCRIPT, 'utf8');
  const yml = await readFile(RELEASE_YML, 'utf8');
  for (const arch of ['x86_64', 'aarch64']) {
    for (const os of ['unknown-linux-gnu', 'apple-darwin']) {
      assert.match(script, new RegExp(`\\$arch-${os}`));
      assert.ok(yml.includes(`target: ${arch}-${os}`), `${arch}-${os} не собирается в release.yml`);
    }
  }
});
