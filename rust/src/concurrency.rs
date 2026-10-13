//! Параллелизм без async-рантайма (RFC 20, решение 2: блокирующий стиль, `std::thread::scope`).
//!
//! Два независимых примитива:
//!
//! * [`AdaptiveSemaphore`] — порт `createAdaptiveSemaphore` (`lib/util.mjs:134-163`):
//!   жёсткий cap + AIMD, при 429 мультипликативное снижение, за серию успехов +1 до max.
//!   Точка вызова 429 в JS — `throttleFor` (`lib/llm.mjs:122-128`), где троттлятся сразу
//!   оба семафора: глобальный (`providerConcurrency`, дефолт 12, `lib/llm.mjs:110`) и
//!   per-model (`providerPerModel`, дефолт 6, `lib/llm.mjs:113`).
//! * [`run_limited`] / [`try_run_limited`] — порт `mapLimit` (`lib/util.mjs:40-48`):
//!   пул из `min(limit, items.len())` воркеров тянет общую очередь, порядок результатов
//!   равен порядку входов.
//!
//! Эталонные тесты JS: `test/pr-review.engine.test.mjs:255-281`.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread;

use serde::Serialize;

/// Множитель снижения лимита при 429 (`lib/util.mjs:158`: `Math.floor(limit * 0.7)`).
pub const THROTTLE_FACTOR: f64 = 0.7;

/// Нижний предел лимита после снижения (`lib/util.mjs:158`: `Math.max(2, …)`).
pub const MIN_LIMIT: usize = 2;

/// Сколько освобождений слота подряд поднимают лимит на [`INCREASE_STEP`]
/// (`lib/util.mjs:154`: `sinceIncrease >= 25`).
pub const SUCCESSES_PER_INCREASE: usize = 25;

/// Шаг повышения лимита (`lib/util.mjs:154`: `limit += 1`).
pub const INCREASE_STEP: usize = 1;

/// Снимок состояния семафора — аналог `stats()` из `lib/util.mjs:161`.
///
/// Логируется при 429 (`lib/llm.mjs:127`), поэтому сериализуется теми же тремя полями.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SemaphoreStats {
    /// Текущий (адаптивный) лимит параллелизма.
    pub limit: usize,
    /// Сколько слотов занято прямо сейчас.
    pub running: usize,
    /// Сколько потоков ждут слота (аналог длины `queue` в JS).
    pub queued: usize,
}

#[derive(Debug)]
struct State {
    limit: usize,
    running: usize,
    waiting: usize,
    since_increase: usize,
}

/// Адаптивный семафор параллелизма провайдера LLM: жёсткий cap + AIMD.
///
/// Отличия от JS-оригинала, о которых стоит знать:
///
/// * Очередь ожидающих в JS — FIFO (`queue.shift()`), здесь пробуждение через `Condvar`
///   и порядок допуска не гарантируется. Порядок допуска не является частью контракта:
///   ни один вызывающий в `lib/llm.mjs` на него не опирается.
/// * `Mutex` при панике держателя не отравляет логику: все блокировки берутся
///   игнорируя poisoning (состояние из целочисленных полей всегда согласовано).
#[derive(Debug)]
pub struct AdaptiveSemaphore {
    hard_max: usize,
    state: Mutex<State>,
    ready: Condvar,
}

impl AdaptiveSemaphore {
    /// Создаёт семафор с жёстким максимумом `max` (`lib/util.mjs:135`: `Math.max(1, max)`).
    pub fn new(max: usize) -> Self {
        let hard_max = max.max(1);
        Self {
            hard_max,
            state: Mutex::new(State {
                limit: hard_max,
                running: 0,
                waiting: 0,
                since_increase: 0,
            }),
            ready: Condvar::new(),
        }
    }

    /// Жёсткий потолок, выше которого лимит не поднимается при росте.
    pub fn hard_max(&self) -> usize {
        self.hard_max
    }

    /// Занимает слот, блокируя поток, пока `running >= limit`.
    ///
    /// Слот освобождается в `Drop` у [`Permit`]; там же засчитывается успех
    /// (ровно как `release()` в `lib/util.mjs:151-156`, который делает и то и другое).
    pub fn acquire(&self) -> Permit<'_> {
        let mut state = lock(&self.state);
        if state.running >= state.limit {
            state.waiting += 1;
            while state.running >= state.limit {
                state = self
                    .ready
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            state.waiting -= 1;
        }
        state.running += 1;
        drop(state);
        Permit { sem: self }
    }

    /// Реакция на 429: `limit = max(MIN_LIMIT, floor(limit * 0.7))`, счётчик успехов в 0
    /// (`lib/util.mjs:157-160`).
    ///
    /// Пол [`MIN_LIMIT`] в JS сильнее жёсткого максимума, поэтому у семафора с `max == 1`
    /// первый же 429 поднимает лимит до 2. Поведение воспроизведено 1:1 намеренно:
    /// расхождение здесь означало бы разный параллелизм у JS- и Rust-путей.
    pub fn on_429(&self) {
        let mut state = lock(&self.state);
        state.limit = MIN_LIMIT.max((state.limit as f64 * THROTTLE_FACTOR).floor() as usize);
        state.since_increase = 0;
        drop(state);
        // Пол MIN_LIMIT мог поднять лимит (случай hard_max < 2) — будим ожидающих.
        self.ready.notify_all();
    }

    /// Тик успешного вызова: каждые [`SUCCESSES_PER_INCREASE`] тиков лимит растёт
    /// на [`INCREASE_STEP`], пока `limit < hard_max` (`lib/util.mjs:153-154`).
    ///
    /// При обычном RAII-использовании звать не нужно: `Drop` у [`Permit`] уже делает тик,
    /// потому что в JS обе половины (освобождение слота и счёт успеха) сидят в одном
    /// `release()`. Метод существует для путей, где слот освобождён раньше вердикта,
    /// и для тестов — двойной вызов на один вызов LLM ускорит рост лимита.
    pub fn on_success(&self) {
        let mut state = lock(&self.state);
        Self::tick_success(&mut state, self.hard_max);
        drop(state);
        self.ready.notify_all();
    }

    /// Текущий адаптивный лимит.
    pub fn current_limit(&self) -> usize {
        lock(&self.state).limit
    }

    /// Снимок `{ limit, running, queued }` — как `stats()` в `lib/util.mjs:161`.
    pub fn stats(&self) -> SemaphoreStats {
        let state = lock(&self.state);
        SemaphoreStats {
            limit: state.limit,
            running: state.running,
            queued: state.waiting,
        }
    }

    fn tick_success(state: &mut State, hard_max: usize) {
        state.since_increase += 1;
        if state.since_increase >= SUCCESSES_PER_INCREASE && state.limit < hard_max {
            state.limit += INCREASE_STEP;
            state.since_increase = 0;
        }
    }

    fn release(&self) {
        let mut state = lock(&self.state);
        state.running = state.running.saturating_sub(1);
        Self::tick_success(&mut state, self.hard_max);
        drop(state);
        // notify_all, а не notify_one: рост лимита может впустить сразу нескольких
        // (в JS это делает цикл в pump(), lib/util.mjs:140-145).
        self.ready.notify_all();
    }
}

/// RAII-владение слотом семафора: слот возвращается в `Drop`.
#[derive(Debug)]
pub struct Permit<'a> {
    sem: &'a AdaptiveSemaphore,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.sem.release();
    }
}

/// Пул воркеров с ограничением параллелизма — порт `mapLimit` (`lib/util.mjs:40-48`).
///
/// Порядок результатов равен порядку входов; одновременно работает не более
/// `min(limit, items.len())` потоков (`limit == 0` трактуется как 1, как `Math.max(1, …)`
/// в JS). Потоки — `std::thread::scope`, поэтому `f` может занимать ссылки на стек
/// вызывающего и async-рантайм не нужен.
///
/// **Паника внутри `f`**: паникующий воркер выходит из цикла, остальные дорабатывают
/// очередь до конца, все потоки джойнятся, и только потом паника пробрасывается
/// вызывающему (`resume_unwind`). Зависания нет; если паник было несколько, наружу
/// уходит первая по порядку джойна, остальные проглатываются.
pub fn run_limited<T, R>(items: Vec<T>, limit: usize, f: impl Fn(T) -> R + Sync) -> Vec<R>
where
    T: Send,
    R: Send,
{
    let total = items.len();
    if total == 0 {
        return Vec::new();
    }
    // JS: Math.max(1, Math.min(limit, items.length)).
    let workers = limit.clamp(1, total);
    let queue = Mutex::new(items.into_iter().enumerate());
    let queue_ref = &queue;
    let f_ref = &f;

    let collected: Vec<Vec<(usize, R)>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(move || {
                    let mut local = Vec::new();
                    loop {
                        // Блокировку держим только на выдаче элемента, не на вызове f.
                        let next = lock(queue_ref).next();
                        match next {
                            Some((idx, item)) => local.push((idx, f_ref(item))),
                            None => break,
                        }
                    }
                    local
                })
            })
            .collect();

        let mut panic_payload = None;
        let mut collected = Vec::with_capacity(workers);
        for handle in handles {
            match handle.join() {
                Ok(local) => collected.push(local),
                Err(payload) => {
                    if panic_payload.is_none() {
                        panic_payload = Some(payload);
                    }
                }
            }
        }
        // Джойн всех воркеров закончен — можно паниковать, никто не остался висеть.
        if let Some(payload) = panic_payload {
            std::panic::resume_unwind(payload);
        }
        collected
    });

    let mut slots: Vec<Option<R>> = (0..total).map(|_| None).collect();
    for local in collected {
        for (idx, value) in local {
            slots[idx] = Some(value);
        }
    }
    slots
        .into_iter()
        .map(|slot| slot.expect("каждый индекс обработан ровно одним воркером"))
        .collect()
}

/// [`run_limited`] для фаллибельной `f`.
///
/// Семантика ошибки повторяет JS: `mapLimit` собирает воркеры в `Promise.all`
/// (`lib/util.mjs:46`), а `Promise.all` отвергается первой ошибкой и **не отменяет**
/// остальную работу — поэтому здесь тоже отрабатывают все элементы, а наружу уходит
/// одна ошибка. Расхождение одно и намеренное: JS отдаёт ту, что отвергла раньше по
/// времени (недетерминированно при равном параллелизме), мы — ошибку наименьшего
/// индекса, чтобы результат прогона был воспроизводимым.
///
/// Нужны все ошибки — зовите [`run_limited`] с `f`, возвращающей `Result`: вернётся
/// `Vec<Result<R, E>>` в порядке входов.
pub fn try_run_limited<T, R, E>(
    items: Vec<T>,
    limit: usize,
    f: impl Fn(T) -> Result<R, E> + Sync,
) -> Result<Vec<R>, E>
where
    T: Send,
    R: Send,
    E: Send,
{
    run_limited(items, limit, f).into_iter().collect()
}

/// Блокировка, игнорирующая poisoning: паника вызывающего не должна ронять пул.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Максимум одновременно работающих: `fetch_max` по счётчику входов/выходов.
    struct Concurrency {
        now: AtomicUsize,
        peak: AtomicUsize,
    }

    impl Concurrency {
        fn new() -> Self {
            Self {
                now: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }
        }

        fn enter(&self) {
            let current = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(current, Ordering::SeqCst);
        }

        fn leave(&self) {
            self.now.fetch_sub(1, Ordering::SeqCst);
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn семафор_блокирует_сверх_лимита_и_будит_после_освобождения() {
        // Детерминированная проверка cap'а: два слота заняты, третий acquire обязан ждать.
        let sem = AdaptiveSemaphore::new(2);
        let first = sem.acquire();
        let second = sem.acquire();
        assert_eq!(sem.stats().running, 2);

        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let _permit = sem.acquire();
                tx.send(()).unwrap();
            });
            assert!(
                rx.recv_timeout(Duration::from_millis(150)).is_err(),
                "третий acquire не должен пройти, пока заняты оба слота"
            );
            drop(first);
            rx.recv_timeout(Duration::from_secs(5))
                .expect("после release ожидающий поток обязан быть разбужен");
        });
        drop(second);
        assert_eq!(sem.stats().running, 0, "все permit'ы вернули слоты");
    }

    #[test]
    fn семафор_не_пускает_больше_лимита_под_нагрузкой() {
        // Аналог JS-теста «semaphore: cap параллелизма соблюдается»
        // (test/pr-review.engine.test.mjs:255).
        let sem = AdaptiveSemaphore::new(3);
        let conc = Concurrency::new();
        thread::scope(|scope| {
            for _ in 0..10 {
                scope.spawn(|| {
                    let _permit = sem.acquire();
                    conc.enter();
                    thread::sleep(Duration::from_millis(10));
                    conc.leave();
                });
            }
        });
        assert!(conc.peak() <= 3, "одновременно не больше лимита: {}", conc.peak());
        assert!(conc.peak() >= 2, "параллелизм действительно был: {}", conc.peak());
    }

    #[test]
    fn on_429_снижает_лимит_на_07_с_полом_2() {
        // 1:1 с JS-тестом «throttle по 429 — ×0.7 floor 2»
        // (test/pr-review.engine.test.mjs:269).
        let sem = AdaptiveSemaphore::new(10);
        sem.on_429();
        assert_eq!(sem.current_limit(), 7);
        sem.on_429();
        assert_eq!(sem.current_limit(), 4);
        for _ in 0..3 {
            sem.on_429();
        }
        assert_eq!(sem.current_limit(), MIN_LIMIT, "пол 2");
    }

    #[test]
    fn серия_успехов_поднимает_лимит_по_одному_до_hard_max() {
        let sem = AdaptiveSemaphore::new(10);
        for _ in 0..5 {
            sem.on_429();
        }
        assert_eq!(sem.current_limit(), 2);

        for _ in 0..(SUCCESSES_PER_INCREASE - 1) {
            sem.on_success();
        }
        assert_eq!(sem.current_limit(), 2, "24 успеха ещё не поднимают лимит");
        sem.on_success();
        assert_eq!(sem.current_limit(), 3, "+1 за 25 успехов");

        for _ in 0..200 {
            sem.on_success();
        }
        assert_eq!(sem.current_limit(), 10, "рост ограничен hard_max");
    }

    #[test]
    fn on_429_сбрасывает_счётчик_успехов() {
        let sem = AdaptiveSemaphore::new(10);
        sem.on_429(); // limit 7
        for _ in 0..(SUCCESSES_PER_INCREASE - 1) {
            sem.on_success();
        }
        sem.on_429(); // limit 4, счётчик обнулён
        assert_eq!(sem.current_limit(), 4);
        for _ in 0..(SUCCESSES_PER_INCREASE - 1) {
            sem.on_success();
        }
        assert_eq!(sem.current_limit(), 4, "накопленные до 429 успехи не засчитываются");
        sem.on_success();
        assert_eq!(sem.current_limit(), 5);
    }

    #[test]
    fn drop_permit_считается_успехом_как_release_в_js() {
        let sem = AdaptiveSemaphore::new(10);
        sem.on_429();
        assert_eq!(sem.current_limit(), 7);
        for _ in 0..SUCCESSES_PER_INCREASE {
            drop(sem.acquire());
        }
        assert_eq!(sem.current_limit(), 8, "Drop делает обе половины JS-release()");
    }

    #[test]
    fn пол_2_сильнее_hard_max_меньше_двух_как_в_js() {
        // Задокументированная причуда JS: Math.max(2, …) не смотрит на hardMax.
        let sem = AdaptiveSemaphore::new(1);
        assert_eq!(sem.current_limit(), 1);
        sem.on_429();
        assert_eq!(sem.current_limit(), 2);
    }

    #[test]
    fn stats_показывает_очередь_ожидающих() {
        let sem = AdaptiveSemaphore::new(1);
        let held = sem.acquire();
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let _permit = sem.acquire();
                tx.send(()).unwrap();
            });
            // Ждём, пока воркер реально встанет в очередь.
            let mut queued = 0;
            for _ in 0..200 {
                queued = sem.stats().queued;
                if queued == 1 {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(queued, 1, "ожидающий поток виден в stats().queued");
            drop(held);
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        assert_eq!(sem.stats().queued, 0);
    }

    #[test]
    fn run_limited_сохраняет_порядок_результатов() {
        let items: Vec<usize> = (0..50).collect();
        let out = run_limited(items, 8, |value| {
            // Обратные задержки: без сборки по индексу порядок бы перемешался.
            thread::sleep(Duration::from_millis((50 - value) as u64 % 7));
            value * 2
        });
        assert_eq!(out, (0..50).map(|v| v * 2).collect::<Vec<_>>());
    }

    #[test]
    fn run_limited_держит_лимит_под_нагрузкой() {
        let conc = Concurrency::new();
        let items: Vec<usize> = (0..40).collect();
        let out = run_limited(items, 4, |value| {
            conc.enter();
            thread::sleep(Duration::from_millis(2));
            conc.leave();
            value
        });
        assert_eq!(out.len(), 40);
        assert!(conc.peak() <= 4, "не больше 4 одновременно: {}", conc.peak());
        assert!(conc.peak() >= 2, "воркеры реально работали параллельно: {}", conc.peak());
    }

    #[test]
    fn run_limited_на_пустом_входе_и_нулевом_лимите() {
        let empty: Vec<usize> = Vec::new();
        assert!(run_limited(empty, 4, |value: usize| value).is_empty());

        // JS: Math.max(1, Math.min(0, len)) === 1 — нулевой лимит это один воркер.
        let conc = Concurrency::new();
        let out = run_limited(vec![1, 2, 3], 0, |value| {
            conc.enter();
            thread::sleep(Duration::from_millis(2));
            conc.leave();
            value
        });
        assert_eq!(out, vec![1, 2, 3]);
        assert_eq!(conc.peak(), 1, "лимит 0 трактуется как 1");
    }

    #[test]
    fn паника_в_элементе_не_вешает_остальные_и_пробрасывается() {
        let done = AtomicUsize::new(0);
        let items: Vec<usize> = (0..8).collect();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_limited(items, 3, |value| {
                if value == 3 {
                    panic!("элемент 3 паникует");
                }
                thread::sleep(Duration::from_millis(2));
                done.fetch_add(1, Ordering::SeqCst);
                value
            })
        }));
        assert!(result.is_err(), "паника воркера пробрасывается вызывающему");
        assert_eq!(
            done.load(Ordering::SeqCst),
            7,
            "остальные 7 элементов доработали до конца, зависания нет"
        );
    }

    #[test]
    fn try_run_limited_отдаёт_ошибку_наименьшего_индекса_выполнив_всё() {
        let done = AtomicUsize::new(0);
        let items: Vec<usize> = (0..10).collect();
        let result: Result<Vec<usize>, String> = try_run_limited(items, 3, |value| {
            done.fetch_add(1, Ordering::SeqCst);
            if value == 7 {
                // Падает раньше по времени, но позже по индексу.
                return Err(format!("ошибка на {value}"));
            }
            if value == 4 {
                thread::sleep(Duration::from_millis(20));
                return Err(format!("ошибка на {value}"));
            }
            Ok(value)
        });
        assert_eq!(result, Err("ошибка на 4".to_string()));
        assert_eq!(done.load(Ordering::SeqCst), 10, "отмены остальных элементов нет");
    }

    #[test]
    fn try_run_limited_на_успехе_сохраняет_порядок() {
        let items: Vec<usize> = (0..20).collect();
        let out: Result<Vec<usize>, String> = try_run_limited(items, 5, |value| Ok(value + 1));
        assert_eq!(out.unwrap(), (1..21).collect::<Vec<_>>());
    }

    #[test]
    fn семафор_и_пул_работают_вместе() {
        // Сценарий из lib/llm.mjs: глобальный семафор поверх пула воркеров.
        let sem = AdaptiveSemaphore::new(2);
        let conc = Concurrency::new();
        let items: Vec<usize> = (0..12).collect();
        let out = run_limited(items, 6, |value| {
            let _permit = sem.acquire();
            conc.enter();
            thread::sleep(Duration::from_millis(3));
            conc.leave();
            value
        });
        assert_eq!(out.len(), 12);
        assert!(conc.peak() <= 2, "семафор режет параллелизм пула: {}", conc.peak());
    }
}
