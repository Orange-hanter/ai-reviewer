#!/usr/bin/env node
/**
 * Печатает тело `run: |` шага workflow по его имени — без YAML-парсера (ноль зависимостей).
 * Нужен CI-джобу consumer-binaries: он исполняет шаг шаблона потребителя как есть, а не его
 * копию, иначе проверялась бы копия.
 *
 *   node test/helpers/extract-step-run.mjs examples/ci/pr-review.yml "Download engine binaries"
 */
import { readFileSync } from 'node:fs';

export function extractStepRun(src, name) {
  const lines = src.split('\n');
  const at = lines.findIndex((l) => l.trim() === `- name: ${name}`);
  if (at < 0) throw new Error(`шаг «${name}» не найден`);
  const stepIndent = lines[at].indexOf('-');
  let i = at + 1;
  for (; i < lines.length; i++) {
    const l = lines[i];
    const ind = l.length - l.trimStart().length;
    if (l.trim() && ind <= stepIndent) throw new Error(`у шага «${name}» нет run: |`);
    if (/^\s*run: \|\s*$/.test(l)) break;
  }
  const runIndent = lines[i].indexOf('run:');
  const body = [];
  for (i += 1; i < lines.length; i++) {
    const l = lines[i];
    const ind = l.length - l.trimStart().length;
    if (l.trim() && ind <= runIndent) break;
    body.push(l);
  }
  const strip = Math.min(...body.filter((l) => l.trim()).map((l) => l.length - l.trimStart().length));
  return `${body.map((l) => l.slice(strip)).join('\n').trimEnd()}\n`;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [file, name] = process.argv.slice(2);
  process.stdout.write(extractStepRun(readFileSync(file, 'utf8'), name));
}
