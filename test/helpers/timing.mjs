/**
 * Бюджет времени для тестов, которые меряют латентность (холодный старт MCP, fail-fast
 * лаунчера, RFC 10). Пороги рассчитаны на обычный CI-раннер. На нагруженной машине они
 * срываются без регресса: сборочный раннер релиза гоняет `npm test` сразу после
 * `cargo build --release`, на macOS свежие бинари сканирует система. Там порог
 * умножается на AI_REVIEWER_TEST_TIMING_SLACK (число ≥ 1, по умолчанию 1), а не
 * выключается: тест по-прежнему ловит зависание и порядок величины.
 */
export function timingSlack(env = process.env) {
  const n = Number(env.AI_REVIEWER_TEST_TIMING_SLACK);
  return Number.isFinite(n) && n >= 1 ? n : 1;
}

export function timingBudget(ms, env = process.env) {
  return Math.round(ms * timingSlack(env));
}
