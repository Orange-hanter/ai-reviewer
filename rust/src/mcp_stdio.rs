//! Общий stdio-транспорт MCP — порт lib/mcp-stdio.mjs (RFC 16, транспортная
//! часть T40, понадобилась T23 раньше самой T40).
//!
//! Кадры Content-Length (LSP-стиль) и «голый» JSON (newline-delimited), частичные
//! чтения, batch, oversized-кадры > 8 МБ, notifications. Доменная логика — в
//! обработчике: модуль ничего не знает ни про structure, ни про ревью.
//!
//! Обработчик: `(request, notify) → Ok(Some(response)) | Ok(None)` (notification
//! без ответа) | `Err` (→ -32603 `internal error: <msg>`, как handleSafely в JS).
//! `notify(method, params)` пишет notification без id.
//!
//! Паритет с JS:
//! - формат ответа выбирается по последнему принятому кадру: Content-Length или
//!   голый JSON + `\n` (переменная `framing`; стартовое значение — lsp);
//! - разбор голого JSON — счётчик глубины по `{[`/`}]` без проверки парности, с
//!   учётом строк и escape; после объекта съедаются необязательные `\r` и `\n`;
//! - заголовки: разделитель `\r\n\r\n` или `\n\n` (какой раньше), имя
//!   Content-Length без учёта регистра, неизвестные заголовки игнорируются,
//!   значение — `Number.parseInt(trim, 10)`, побеждает последний валидный; кадр
//!   без длины выбрасывается вместе с заголовком;
//! - oversized: заголовок снят, тело пропускается по мере поступления, затем
//!   ответ -32600 `message exceeds 8388608 bytes`, поток остаётся синхронным;
//! - заголовок без конца длиннее 16 КБ — буфер сбрасывается;
//! - пустой batch / не-объект → -32600 `invalid request`, битый JSON → -32700
//!   `parse error`, элементы batch-а — по одному, notifications в batch-ответ не
//!   попадают, пустой итог batch-а не пишется вовсе;
//! - SIGINT/SIGTERM → выход 0 после дописывания текущего ответа (в JS — после
//!   `drain`): обработчик сигнала только ставит флаг, а `siginterrupt` заставляет
//!   блокирующий `read` вернуть EINTR, после чего цикл выходит;
//! - EOF stdin → выход; недочитанный кадр и незавершённое поглощение oversized
//!   молча теряются (как в JS — известный кейс `mcp-oversized-eof-tail`).
//!
//! Фоновые notifications ([`SharedOut`]): в JS `notify` пишет в `process.stdout`
//! в любой момент, в том числе из промиса, который живёт после ответа на запрос
//! (`review_run` с `wait: false`). Здесь цикл держит выход у себя, поэтому для
//! таких писателей выход и текущий формат кадра разделяются через `SharedOut`:
//! каждое сообщение пишется одним `write_all` под мьютексом и не перемешивается
//! с ответом цикла.
//!
//! Отклонения:
//! - JSON.parse без лимита вложенности, serde_json — с лимитом 128: более глубокий
//!   запрос получает -32700 вместо разбора (защитное поведение, валидные MCP-
//!   запросы так глубоко не вкладываются); одиночные суррогаты `\ud800` в строках
//!   serde_json тоже отвергает (-32700), V8 принимает;
//! - запись в закрытый stdout (EPIPE) завершает цикл с выходом 0 — в Node это
//!   необработанное событие `error` потока.

use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{json, Value};

use crate::report::js_json_stringify;

/// Предел тела одного кадра (MAX_MESSAGE_BYTES).
pub const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Предел заголовка без найденного конца (MAX_HEADER_BYTES).
pub const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Формат кадра на проводе.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `Content-Length: N\r\n\r\n<body>`.
    Lsp,
    /// Голый JSON, ответ — строка + `\n`.
    Json,
}

/// Извлечённое из буфера сообщение: текст или маркер пропущенного oversized-кадра.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// `None` — oversized-кадр, тело отброшено.
    pub text: Option<String>,
    /// `None` — формат не меняется (oversized наследует текущий).
    pub framing: Option<Framing>,
}

/// Инкрементальный разборщик входного потока (takeMessages из JS).
#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
    skip_bytes: u64,
}

enum Bare {
    Wait,
    NotJson,
    Message { start: usize, end: usize, consume: usize },
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Пробельные символы String.prototype.trim в пределах latin1
/// (заголовок декодируется как latin1, поэтому символов выше U+00FF нет).
fn js_trim_latin1(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}'))
}

/// `Number.parseInt(s, 10)` + `Number.isInteger(n) && n >= 0` → длина кадра.
/// Ведущие цифры после необязательного знака; без цифр — NaN (None).
fn parse_content_length(raw: &str) -> Option<u64> {
    let s = js_trim_latin1(raw);
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let lead: String = digits.chars().take_while(char::is_ascii_digit).collect();
    if lead.is_empty() {
        return None;
    }
    // f64, как Number: огромное значение не переполняется, а уходит в oversized.
    let value: f64 = lead.parse().ok()?;
    if negative && value != 0.0 {
        return None;
    }
    Some(if value >= u64::MAX as f64 { u64::MAX } else { value as u64 })
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Дописать прочитанный кусок stdin.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    fn take_bare_json(&self) -> Bare {
        let buf = &self.buffer;
        let mut i = 0;
        while i < buf.len() && is_ws(buf[i]) {
            i += 1;
        }
        if i >= buf.len() {
            return Bare::Wait;
        }
        if buf[i] != b'{' && buf[i] != b'[' {
            return Bare::NotJson;
        }
        let mut depth: i64 = 0;
        let mut in_str = false;
        let mut escape = false;
        for (j, &c) in buf.iter().enumerate().skip(i) {
            if in_str {
                if escape {
                    escape = false;
                } else if c == b'\\' {
                    escape = true;
                } else if c == b'"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                b'"' => in_str = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        let end = j + 1;
                        let mut consume = end;
                        if buf.get(consume) == Some(&b'\r') {
                            consume += 1;
                        }
                        if buf.get(consume) == Some(&b'\n') {
                            consume += 1;
                        }
                        return Bare::Message { start: i, end, consume };
                    }
                }
                _ => {}
            }
        }
        Bare::Wait
    }

    /// Все целиком пришедшие сообщения; хвост остаётся в буфере.
    pub fn take_messages(&mut self) -> Vec<Frame> {
        let mut messages = Vec::new();
        loop {
            if self.skip_bytes > 0 {
                let drop = (self.skip_bytes.min(self.buffer.len() as u64)) as usize;
                self.buffer.drain(..drop);
                self.skip_bytes -= drop as u64;
                if self.skip_bytes > 0 {
                    break;
                }
                messages.push(Frame { text: None, framing: None });
                continue;
            }
            match self.take_bare_json() {
                Bare::Message { start, end, consume } => {
                    let text = String::from_utf8_lossy(&self.buffer[start..end]).into_owned();
                    self.buffer.drain(..consume);
                    messages.push(Frame { text: Some(text), framing: Some(Framing::Json) });
                    continue;
                }
                Bare::Wait => break,
                Bare::NotJson => {}
            }
            let crlf = find(&self.buffer, b"\r\n\r\n");
            let lf = find(&self.buffer, b"\n\n");
            let (header_end, header_width) = match (crlf, lf) {
                (Some(c), l) if l.map_or(true, |l| c <= l) => (c, 4),
                (_, Some(l)) => (l, 2),
                _ => {
                    if self.buffer.len() > MAX_HEADER_BYTES {
                        self.buffer.clear();
                    }
                    break;
                }
            };
            // latin1: каждый байт — символ U+0000..U+00FF.
            let header: String = self.buffer[..header_end].iter().map(|&b| b as char).collect();
            let mut length: Option<u64> = None;
            for raw_line in header.split('\n') {
                let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
                let Some(colon) = line.find(':') else { continue };
                if js_trim_latin1(&line[..colon]).to_lowercase() != "content-length" {
                    continue;
                }
                if let Some(parsed) = parse_content_length(&line[colon + 1..]) {
                    length = Some(parsed);
                }
            }
            let body_start = header_end + header_width;
            let Some(length) = length else {
                self.buffer.drain(..body_start);
                continue;
            };
            if length > MAX_MESSAGE_BYTES {
                self.buffer.drain(..body_start);
                self.skip_bytes = length;
                continue;
            }
            let length = length as usize;
            if self.buffer.len() < body_start + length {
                break;
            }
            let text =
                String::from_utf8_lossy(&self.buffer[body_start..body_start + length]).into_owned();
            self.buffer.drain(..body_start + length);
            messages.push(Frame { text: Some(text), framing: Some(Framing::Lsp) });
        }
        messages
    }
}

/// Сериализация исходящего сообщения в текущем формате.
pub fn encode_message(payload: &Value, framing: Framing) -> Vec<u8> {
    let mut json = String::new();
    js_json_stringify(payload, &mut json);
    match framing {
        Framing::Json => {
            json.push('\n');
            json.into_bytes()
        }
        Framing::Lsp => {
            let mut out = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
            out.extend_from_slice(json.as_bytes());
            out
        }
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn is_request_object(v: &Value) -> bool {
    v.is_object()
}

/// Обработчик домена: запрос + notify → ответ (None — notification).
pub type Notify<'a> = dyn Fn(&str, Value) + 'a;

/// Выход сервера, разделяемый с фоновыми потоками: писатель под мьютексом и
/// формат последнего принятого кадра. Клонируется дёшево (`Arc`).
#[derive(Clone)]
pub struct SharedOut {
    w: Arc<Mutex<Box<dyn Write + Send>>>,
    framing: Arc<AtomicU8>,
}

impl SharedOut {
    pub fn new(w: Box<dyn Write + Send>) -> Self {
        Self { w: Arc::new(Mutex::new(w)), framing: Arc::new(AtomicU8::new(framing_code(Framing::Lsp))) }
    }

    /// Выход в stdout процесса.
    pub fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }

    pub fn framing(&self) -> Framing {
        if self.framing.load(Ordering::SeqCst) == framing_code(Framing::Json) {
            Framing::Json
        } else {
            Framing::Lsp
        }
    }

    fn set_framing(&self, framing: Framing) {
        self.framing.store(framing_code(framing), Ordering::SeqCst);
    }

    /// notification без id в текущем формате; ошибка записи глотается
    /// (закрытый stdout заметит и завершит цикл сам сервер).
    pub fn notify(&self, method: &str, params: Value) {
        let bytes = encode_message(&json!({ "jsonrpc": "2.0", "method": method, "params": params }), self.framing());
        let mut w = self.lock();
        let _ = w.write_all(&bytes).and_then(|()| w.flush());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Box<dyn Write + Send>> {
        // Паника писателя посреди записи не делает выход непригодным: продолжаем.
        self.w.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for SharedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedOut").field("framing", &self.framing()).finish_non_exhaustive()
    }
}

impl Write for &SharedOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.lock().write(buf)
    }
    /// Одно сообщение — один захват мьютекса: кадры разных потоков не перемешиваются.
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.lock().write_all(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.lock().flush()
    }
}

impl Write for SharedOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self).write(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        (&*self).write_all(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&*self).flush()
    }
}

fn framing_code(framing: Framing) -> u8 {
    match framing {
        Framing::Lsp => 0,
        Framing::Json => 1,
    }
}

/// Сервер поверх произвольных Read/Write (stdio в бою, буферы в тестах).
pub struct McpServer<W: Write, H> {
    out: std::cell::RefCell<W>,
    framing: std::cell::Cell<Framing>,
    /// Ошибка записи (EPIPE): цикл завершается.
    broken: std::cell::Cell<bool>,
    /// Копия формата для фоновых писателей ([`SharedOut`]).
    mirror: Option<SharedOut>,
    handler: H,
}

impl<W, H> McpServer<W, H>
where
    W: Write,
    H: FnMut(&Value, &Notify<'_>) -> Result<Option<Value>>,
{
    pub fn new(out: W, handler: H) -> Self {
        Self {
            out: std::cell::RefCell::new(out),
            framing: std::cell::Cell::new(Framing::Lsp),
            broken: std::cell::Cell::new(false),
            mirror: None,
            handler,
        }
    }

    /// Формат каждого принятого кадра дублируется в `mirror`: notifications
    /// фоновых потоков идут в том же формате, что ответы цикла.
    pub fn with_framing_mirror(mut self, mirror: SharedOut) -> Self {
        self.mirror = Some(mirror);
        self
    }

    pub fn into_writer(self) -> W {
        self.out.into_inner()
    }

    fn write_message(&self, payload: &Value) {
        let bytes = encode_message(payload, self.framing.get());
        let mut out = self.out.borrow_mut();
        if out.write_all(&bytes).and_then(|()| out.flush()).is_err() {
            self.broken.set(true);
        }
    }

    fn handle_safely(&mut self, request: &Value) -> Option<Value> {
        let out = &self.out;
        let framing = &self.framing;
        let broken = &self.broken;
        let notify = |method: &str, params: Value| {
            let bytes = encode_message(
                &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
                framing.get(),
            );
            let mut w = out.borrow_mut();
            if w.write_all(&bytes).and_then(|()| w.flush()).is_err() {
                broken.set(true);
            }
        };
        match (self.handler)(request, &notify) {
            Ok(response) => response,
            Err(error) => {
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                Some(error_response(id, -32603, &format!("internal error: {error}")))
            }
        }
    }

    fn dispatch_message(&mut self, text: &str) {
        let request: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => {
                self.write_message(&error_response(Value::Null, -32700, "parse error"));
                return;
            }
        };
        if let Value::Array(items) = &request {
            if items.is_empty() {
                self.write_message(&error_response(Value::Null, -32600, "invalid request"));
                return;
            }
            let mut responses = Vec::new();
            for item in items {
                if !is_request_object(item) {
                    responses.push(error_response(Value::Null, -32600, "invalid request"));
                    continue;
                }
                if let Some(response) = self.handle_safely(item) {
                    responses.push(response);
                }
            }
            if !responses.is_empty() {
                self.write_message(&Value::Array(responses));
            }
            return;
        }
        if !is_request_object(&request) {
            self.write_message(&error_response(Value::Null, -32600, "invalid request"));
            return;
        }
        if let Some(response) = self.handle_safely(&request) {
            self.write_message(&response);
        }
    }

    /// Обработать кадры, накопившиеся в reader. false — запись сломана, пора выходить.
    pub fn process(&mut self, reader: &mut FrameReader) -> bool {
        for frame in reader.take_messages() {
            if let Some(framing) = frame.framing {
                self.framing.set(framing);
                if let Some(mirror) = &self.mirror {
                    mirror.set_framing(framing);
                }
            }
            match frame.text {
                None => self.write_message(&error_response(
                    Value::Null,
                    -32600,
                    &format!("message exceeds {MAX_MESSAGE_BYTES} bytes"),
                )),
                Some(text) => self.dispatch_message(&text),
            }
            if self.broken.get() {
                return false;
            }
        }
        true
    }

    /// Цикл чтения до EOF / сигнала / сломанной записи.
    pub fn serve(&mut self, input: &mut impl Read, stop: &AtomicBool) -> Result<()> {
        let mut reader = FrameReader::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            let n = match input.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            reader.push(&chunk[..n]);
            if !self.process(&mut reader) {
                return Ok(());
            }
        }
    }
}

/// Флаг «пришёл SIGINT/SIGTERM».
static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
mod signals {
    use super::STOP;
    use std::sync::atomic::Ordering;
    use std::sync::Once;
    use std::time::Duration;

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
        fn siginterrupt(sig: i32, flag: i32) -> i32;
    }

    extern "C" fn on_signal(_sig: i32) {
        // async-signal-safe: только атомарная запись
        STOP.store(true, Ordering::SeqCst);
    }

    /// Сколько ждать штатного выхода (EINTR → `serve` возвращается → `main`), прежде чем
    /// завершить процесс принудительно.
    const GRACE: Duration = Duration::from_millis(150);
    const POLL: Duration = Duration::from_millis(20);

    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            for sig in [SIGINT, SIGTERM] {
                // SAFETY: обработчик только пишет в AtomicBool; siginterrupt(…, 1)
                // снимает SA_RESTART, чтобы блокирующий read вернул EINTR.
                unsafe {
                    signal(sig, on_signal);
                    siginterrupt(sig, 1);
                }
            }
            // Сторож: флаг STOP цикл `serve` видит только между чтениями stdin. Пока главный
            // поток занят синхронным обработчиком (review_run с wait:true — минуты MoA), он
            // его не проверит, и процесс жил бы до конца прогона. В JS обработчик сигнала —
            // `process.exit(0)` сразу: цикл событий во время await свободен. Сторож даёт ту
            // же семантику: штатный выход получает GRACE, затем exit(0). Сигнал может прийти
            // и в этот поток, а не в главный, — тогда EINTR в read не случится, и выход
            // тоже обеспечивает сторож.
            let _ = std::thread::Builder::new().name("mcp-signal-watch".into()).spawn(|| {
                while !STOP.load(Ordering::SeqCst) {
                    std::thread::sleep(POLL);
                }
                std::thread::sleep(GRACE);
                // process::exit сбрасывает буфер stdout (rt::cleanup), как drain в JS.
                std::process::exit(0);
            });
        });
    }
}

/// runMcpStdio: stdio-сервер с обработчиком домена. Возвращается на EOF stdin,
/// после SIGINT/SIGTERM или при закрытом stdout.
pub fn run_mcp_stdio<H>(handler: H) -> Result<()>
where
    H: FnMut(&Value, &Notify<'_>) -> Result<Option<Value>>,
{
    #[cfg(unix)]
    signals::install();
    let stdout = std::io::stdout();
    let mut server = McpServer::new(stdout.lock(), handler);
    let stdin = std::io::stdin();
    // Без BufReader: каждый read — один системный вызов, EINTR доходит до цикла.
    let mut input = stdin.lock();
    server.serve(&mut input, &STOP)
}

/// Пришёл ли SIGINT/SIGTERM (после выхода из цикла: EOF или сигнал).
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// runMcpStdio поверх разделяемого выхода: ответы цикла и фоновые notifications
/// идут через один `SharedOut`.
pub fn run_mcp_stdio_shared<H>(out: SharedOut, handler: H) -> Result<()>
where
    H: FnMut(&Value, &Notify<'_>) -> Result<Option<Value>>,
{
    #[cfg(unix)]
    signals::install();
    let mut server = McpServer::new(out.clone(), handler).with_framing_mirror(out);
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    server.serve(&mut input, &STOP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lsp(body: &str) -> Vec<u8> {
        let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(body.as_bytes());
        out
    }

    fn echo(req: &Value, _n: &Notify<'_>) -> Result<Option<Value>> {
        if req.get("method") == Some(&json!("note")) {
            return Ok(None);
        }
        if req.get("method") == Some(&json!("boom")) {
            anyhow::bail!("kaput");
        }
        Ok(Some(json!({ "jsonrpc": "2.0", "id": req.get("id").cloned().unwrap_or(Value::Null), "result": {} })))
    }

    fn run(input: &[u8]) -> String {
        let mut server = McpServer::new(Vec::new(), echo);
        let stop = AtomicBool::new(false);
        server.serve(&mut &input[..], &stop).unwrap();
        String::from_utf8(server.into_writer()).unwrap()
    }

    #[test]
    fn parse_content_length_follows_parse_int() {
        assert_eq!(parse_content_length(" 12abc "), Some(12));
        assert_eq!(parse_content_length("+5"), Some(5));
        assert_eq!(parse_content_length("-0"), Some(0));
        assert_eq!(parse_content_length("-5"), None);
        assert_eq!(parse_content_length("abc"), None);
        assert_eq!(parse_content_length("0x10"), Some(0));
        assert_eq!(parse_content_length("99999999999999999999999"), Some(u64::MAX));
    }

    #[test]
    fn lsp_frames_split_reads_and_unknown_headers() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let mut framed = format!("content-length: {}\r\nX-Unknown: x\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(body.as_bytes());
        let mut reader = FrameReader::new();
        reader.push(&framed[..10]);
        assert!(reader.take_messages().is_empty());
        reader.push(&framed[10..]);
        let frames = reader.take_messages();
        assert_eq!(frames, vec![Frame { text: Some(body.into()), framing: Some(Framing::Lsp) }]);
    }

    #[test]
    fn lf_only_header_and_missing_length() {
        let mut reader = FrameReader::new();
        reader.push(b"X-A: 1\n\nContent-Length: 2\n\n{}");
        let frames = reader.take_messages();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].text.as_deref(), Some("{}"));
    }

    #[test]
    fn bare_json_switches_framing_to_newline() {
        let out = run(b"{\"id\":1,\"method\":\"ping\",\"s\":\"}]\\\"\"}\r\n[{\"id\":2}]");
        assert_eq!(
            out,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n[{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}]\n"
        );
    }

    #[test]
    fn batches_invalid_and_parse_errors() {
        let mut input = Vec::new();
        input.extend(lsp(r#"[{"id":2},42,{"method":"note"}]"#));
        input.extend(lsp("[]"));
        input.extend(lsp("42"));
        input.extend(lsp("{not json"));
        input.extend(lsp(r#"[{"method":"note"}]"#));
        input.extend(lsp(r#"{"id":7,"method":"boom"}"#));
        let out = run(&input);
        let expected = [
            r#"[{"jsonrpc":"2.0","id":2,"result":{}},{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}]"#,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}"#,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}}"#,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#,
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32603,"message":"internal error: kaput"}}"#,
        ];
        let want: String = expected
            .iter()
            .map(|b| String::from_utf8(lsp(b)).unwrap())
            .collect();
        assert_eq!(out, want);
    }

    #[test]
    fn oversized_frame_is_skipped_and_stream_stays_in_sync() {
        let mut reader = FrameReader::new();
        reader.push(format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_BYTES + 1).as_bytes());
        assert!(reader.take_messages().is_empty());
        reader.push(&vec![b'x'; MAX_MESSAGE_BYTES as usize]);
        assert!(reader.take_messages().is_empty());
        let mut tail = vec![b'x'];
        tail.extend(lsp("{}"));
        reader.push(&tail);
        let frames = reader.take_messages();
        assert_eq!(frames[0], Frame { text: None, framing: None });
        assert_eq!(frames[1].text.as_deref(), Some("{}"));
    }

    #[test]
    fn endless_header_is_dropped_after_16k() {
        let mut reader = FrameReader::new();
        reader.push(&vec![b'a'; MAX_HEADER_BYTES + 1]);
        assert!(reader.take_messages().is_empty());
        reader.push(&lsp("{}"));
        assert_eq!(reader.take_messages()[0].text.as_deref(), Some("{}"));
    }

    #[test]
    fn notify_writes_notification_in_current_framing() {
        let handler = |req: &Value, notify: &Notify<'_>| -> Result<Option<Value>> {
            notify("log", json!({ "m": 1 }));
            Ok(Some(json!({ "id": req["id"] })))
        };
        let mut server = McpServer::new(Vec::new(), handler);
        server.serve(&mut &b"{\"id\":1}"[..], &AtomicBool::new(false)).unwrap();
        let out = String::from_utf8(server.into_writer()).unwrap();
        assert_eq!(out, "{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"params\":{\"m\":1}}\n{\"id\":1}\n");
    }
}
