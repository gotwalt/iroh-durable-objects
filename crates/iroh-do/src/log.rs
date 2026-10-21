//! Structured logs: a `tracing` layer that writes each event as one flat JSON object
//! (design note 010).
//!
//! Workers Logs indexes the fields of an object passed to `console.log`, so every event,
//! from this crate, from `iroh-do-core` or from iroh itself, becomes one object with these
//! keys:
//!
//! - `level`: `error`, `warn`, `info`, `debug` or `trace`;
//! - `target`: the event's target, normally its module path;
//! - `message`: the event's message, or the value of its `event` field if it has none;
//! - `spans`: the names of the enclosing spans, outermost first, joined with `:`;
//! - the fields of every enclosing span, then the event's own fields. An inner span's field
//!   replaces an outer one of the same name, and an event field replaces both.
//!
//! Events this repository emits for operators carry a stable `event` field; design note 010
//! lists them. Which events are written is set by `LOG_LEVEL` ([`DEFAULT_LOG_LEVEL`]).
//!
//! The layer builds natively so that its output can be tested; the Worker installs it with a
//! writer that calls `console.log`, `console.warn` or `console.error`.

use std::fmt;

use serde_json::{Map, Value};
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{
    Layer, filter::Targets, layer::Context, prelude::*, registry::LookupSpan,
};

/// `LOG_LEVEL` when the binding is unset or does not parse.
///
/// The syntax is a comma-separated list of `level` and `target=level` directives, the subset
/// of `tracing_subscriber::EnvFilter` syntax without span and field filters. A target matches
/// every target it is a prefix of, and the longest matching directive wins, so `iroh=warn`
/// also covers `iroh_relay`, and `iroh_do=info` takes this crate and `iroh_do_core` back to
/// `info`. `netwatch` warns on every endpoint bind in workerd, where `navigator.onLine` does
/// not exist.
pub const DEFAULT_LOG_LEVEL: &str = "info,iroh=warn,noq=warn,netwatch=error,iroh_do=info";

/// Writes one serialized event. Receives the event's level and its JSON text.
pub type Write = fn(Level, &str);

/// The JSON layer. See the [module docs](self).
pub struct JsonLayer {
    write: Write,
}

impl JsonLayer {
    /// Creates a layer that passes each serialized event to `write`.
    pub fn new(write: Write) -> Self {
        Self { write }
    }
}

/// Builds the subscriber: the JSON layer filtered by `directives`.
///
/// Returns the parse error as a string if `directives` is invalid; the subscriber then uses
/// [`DEFAULT_LOG_LEVEL`].
pub fn subscriber(
    directives: &str,
    write: Write,
) -> (impl Subscriber + Send + Sync + 'static, Option<String>) {
    let (filter, error) = match directives.parse::<Targets>() {
        Ok(t) if !directives.trim().is_empty() => (t, None),
        Ok(_) => (default_filter(), None),
        Err(e) => (default_filter(), Some(e.to_string())),
    };
    let sub = tracing_subscriber::registry()
        .with(JsonLayer::new(write))
        .with(filter);
    (sub, error)
}

fn default_filter() -> Targets {
    DEFAULT_LOG_LEVEL.parse().expect("DEFAULT_LOG_LEVEL parses")
}

/// Fields recorded on a span, kept in its extensions.
struct SpanFields(Map<String, Value>);

struct Fields<'a>(&'a mut Map<String, Value>);

impl Fields<'_> {
    fn put(&mut self, field: &Field, value: Value) {
        self.0.insert(field.name().to_owned(), value);
    }
}

impl Visit for Fields<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::from(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let mut text = value.to_string();
        let mut source = value.source();
        while let Some(s) = source {
            text.push_str(": ");
            text.push_str(&s.to_string());
            source = s.source();
        }
        self.put(field, Value::from(text));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, Value::from(format!("{value:?}")));
    }
}

impl<S> Layer<S> for JsonLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut map = Map::new();
        attrs.record(&mut Fields(&mut map));
        span.extensions_mut().insert(SpanFields(map));
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        if let Some(SpanFields(map)) = ext.get_mut::<SpanFields>() {
            values.record(&mut Fields(map));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut obj = Map::new();
        if let Some(scope) = ctx.event_scope(event) {
            let mut names = String::new();
            for span in scope.from_root() {
                if !names.is_empty() {
                    names.push(':');
                }
                names.push_str(span.name());
                if let Some(SpanFields(map)) = span.extensions().get::<SpanFields>() {
                    obj.extend(map.iter().map(|(k, v)| (k.clone(), v.clone())));
                }
            }
            obj.insert("spans".into(), Value::from(names));
        }
        event.record(&mut Fields(&mut obj));
        if !obj.contains_key("message")
            && let Some(name) = obj.get("event").cloned()
        {
            obj.insert("message".into(), name);
        }
        let level = *meta.level();
        obj.insert("level".into(), Value::from(level_str(level)));
        obj.insert("target".into(), Value::from(meta.target()));
        (self.write)(level, &Value::Object(obj).to_string());
    }
}

fn level_str(level: Level) -> &'static str {
    match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static OUT: Mutex<Vec<(Level, String)>> = Mutex::new(Vec::new());

    fn capture(level: Level, line: &str) {
        OUT.lock().unwrap().push((level, line.to_owned()));
    }

    /// Runs `f` under a subscriber built from `directives`; returns the parsed lines.
    fn run(directives: &str, f: impl FnOnce()) -> Vec<(Level, Value)> {
        let (sub, err) = subscriber(directives, capture);
        assert!(err.is_none(), "{err:?}");
        OUT.lock().unwrap().clear();
        tracing::subscriber::with_default(sub, f);
        OUT.lock()
            .unwrap()
            .drain(..)
            .map(|(l, s)| (l, serde_json::from_str(&s).unwrap()))
            .collect()
    }

    // The tests share `OUT`, so they run one at a time through this lock.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn flattens_span_and_event_fields() {
        let _s = SERIAL.lock().unwrap();
        let lines = run("info", || {
            let outer = tracing::info_span!("node", endpoint = "abc", n = 1);
            let _o = outer.enter();
            let inner = tracing::info_span!("conn", peer = "xyz", n = 2);
            let _i = inner.enter();
            tracing::info!(
                event = "conn_open",
                alpn = "iroh-do/room/1",
                ok = true,
                ms = 3u64
            );
        });
        assert_eq!(lines.len(), 1);
        let (level, v) = &lines[0];
        assert_eq!(*level, Level::INFO);
        assert_eq!(v["event"], "conn_open");
        assert_eq!(v["message"], "conn_open");
        assert_eq!(v["endpoint"], "abc");
        assert_eq!(v["peer"], "xyz");
        assert_eq!(v["n"], 2, "inner span field wins");
        assert_eq!(v["ok"], true);
        assert_eq!(v["ms"], 3);
        assert_eq!(v["spans"], "node:conn");
        assert_eq!(v["level"], "info");
        assert!(v["target"].as_str().unwrap().starts_with("iroh_do::log"));
    }

    #[test]
    fn message_and_levels() {
        let _s = SERIAL.lock().unwrap();
        let lines = run("info", || {
            tracing::warn!(event = "x", "something {}", 1);
            tracing::error!(error = %"boom", "failed");
            tracing::debug!("filtered out");
        });
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, Level::WARN);
        assert_eq!(lines[0].1["message"], "something 1");
        assert_eq!(lines[1].1["level"], "error");
        assert_eq!(lines[1].1["error"], "boom");
    }

    #[test]
    fn default_directives() {
        let _s = SERIAL.lock().unwrap();
        let t: Targets = DEFAULT_LOG_LEVEL.parse().unwrap();
        assert!(t.would_enable("iroh_do::glue", &Level::INFO));
        assert!(t.would_enable("iroh_do_core", &Level::INFO));
        assert!(t.would_enable("worker", &Level::INFO));
        assert!(!t.would_enable("iroh::endpoint", &Level::INFO));
        assert!(!t.would_enable("iroh_relay", &Level::INFO));
        assert!(t.would_enable("iroh::endpoint", &Level::WARN));
        assert!(!t.would_enable("noq_proto::connection", &Level::INFO));
        assert!(!t.would_enable("iroh_do_core", &Level::DEBUG));
        assert!(!t.would_enable("netwatch::interfaces", &Level::WARN));
    }

    #[test]
    fn invalid_directives_fall_back() {
        let (_, err) = subscriber("info,iroh=loud", capture);
        assert!(err.is_some());
        let (_, err) = subscriber("", capture);
        assert!(err.is_none());
    }

    /// A secret key passed to a log macro by mistake prints as `SecretKey(..)`.
    #[test]
    fn secret_key_is_redacted() {
        let _s = SERIAL.lock().unwrap();
        let key = iroh_base::SecretKey::from_bytes(&[7u8; 32]);
        let hex = data_encoding::HEXLOWER.encode(&key.to_bytes());
        let lines = run("info", || tracing::info!(key = ?key, "oops"));
        let text = lines[0].1.to_string();
        assert!(!text.contains(&hex), "{text}");
        assert!(text.contains("SecretKey(..)"));
    }

    /// No log macro in this crate or in `iroh-do-core` passes a value whose name mentions a
    /// secret: the endpoint key, its hex form in the mint request, or the admin token. String
    /// literals are skipped, so a message may name a binding.
    #[test]
    fn no_log_macro_names_a_secret() {
        const FORBIDDEN: &[&str] = &["secret", "token", "authorization", "expected"];
        const MACROS: &[&str] = &[
            "trace!(", "debug!(", "info!(", "warn!(", "error!(", "console_",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for dir in ["src", "src/glue", "../iroh-do-core/src"] {
            for e in std::fs::read_dir(root.join(dir)).unwrap() {
                let p = e.unwrap().path();
                if p.extension().is_some_and(|x| x == "rs") {
                    files.push(p);
                }
            }
        }
        assert!(files.len() > 10);
        let mut checked = 0;
        for path in files {
            let src = std::fs::read_to_string(&path).unwrap();
            // The tests in this module mention a key on purpose.
            let src = src.split("#[cfg(test)]").next().unwrap();
            for m in MACROS {
                for (start, _) in src.match_indices(m) {
                    let call = macro_call(&src[start..]);
                    let lower = strip_literals(call).to_lowercase();
                    for f in FORBIDDEN {
                        assert!(
                            !lower.contains(f),
                            "{}: log call mentions {f:?}: {call}",
                            path.display()
                        );
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 40, "found only {checked} log calls");
    }

    fn strip_literals(s: &str) -> String {
        let mut out = String::new();
        let mut in_str = false;
        let mut escaped = false;
        for c in s.chars() {
            if in_str {
                match (escaped, c) {
                    (false, '\\') => escaped = true,
                    (false, '"') => in_str = false,
                    _ => escaped = false,
                }
            } else if c == '"' {
                in_str = true;
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The text of a macro call starting at `s`, up to its closing parenthesis.
    fn macro_call(s: &str) -> &str {
        let mut depth = 0;
        for (i, c) in s.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return &s[..=i];
                    }
                }
                _ => {}
            }
        }
        s
    }
}
