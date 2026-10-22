//! The endpoint list (design note 009): the registry's schema, and the page and JSON the
//! router serves at `admin.<zone>`.
//!
//! Cloudflare lists Durable Objects only by internal id, so the EndpointIds are recorded in
//! one `registry` object, named [`REGISTRY_NAME`], when they are minted. An endpoint
//! minted before the registry existed registers itself on its next wake.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Name of the one registry object. Not valid z-base-32, so it cannot be an EndpointId.
pub const REGISTRY_NAME: &str = "registry";
/// Longest label, in characters.
pub const MAX_LABEL: usize = 64;
/// Most endpoints the list returns, newest first.
pub const LIST_LIMIT: usize = 500;

/// The registry's tables, created idempotently on every construction.
pub const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS endpoints(
        id TEXT PRIMARY KEY,
        app TEXT NOT NULL,
        label TEXT NOT NULL,
        minted_at INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS endpoints_minted ON endpoints(minted_at)",
];
/// Records an endpoint; binds `id, app, label, minted_at`. A second registration of the same
/// id, such as a backfill racing a mint, keeps the first.
pub const REGISTER: &str = "INSERT INTO endpoints(id, app, label, minted_at) VALUES (?, ?, ?, ?)
 ON CONFLICT(id) DO NOTHING";

/// Removes one endpoint from the list (it was destroyed).
pub const UNREGISTER: &str = "DELETE FROM endpoints WHERE id = ?";
/// The newest `?` endpoints.
pub const LIST: &str =
    "SELECT id, app, label, minted_at FROM endpoints ORDER BY minted_at DESC LIMIT ?";

/// One registered endpoint, as stored and as `/endpoints.json` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// EndpointId, z-base-32.
    pub id: String,
    /// Name of the app the endpoint runs.
    pub app: String,
    /// Label given at mint; empty if none.
    pub label: String,
    /// Mint time in milliseconds since the UNIX epoch.
    pub minted_at: i64,
}

/// Body of `POST admin.<zone>/mint`; the body may also be empty.
#[derive(Debug, Default, Deserialize)]
pub struct MintBody {
    /// Name of the app the endpoint runs; the deployment's first app if absent.
    #[serde(default)]
    pub app: Option<String>,
    /// Optional label shown on the list.
    #[serde(default)]
    pub label: Option<String>,
}

/// Returns the app and label of a mint request body, or an error message.
///
/// An empty body names no app and no label. Labels longer than [`MAX_LABEL`] characters or
/// containing control characters are refused.
pub fn mint_body(body: &str) -> Result<(Option<String>, String), String> {
    if body.trim().is_empty() {
        return Ok((None, String::new()));
    }
    let b: MintBody = serde_json::from_str(body)
        .map_err(|e| format!("body must be JSON {{\"app\":…, \"label\":…}}: {e}"))?;
    let label = b.label.unwrap_or_default().trim().to_string();
    if label.chars().count() > MAX_LABEL {
        return Err(format!("label longer than {MAX_LABEL} characters"));
    }
    if label.chars().any(char::is_control) {
        return Err("label contains control characters".to_string());
    }
    Ok((b.app, label))
}

/// Escapes text for HTML element content and quoted attribute values.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Formats milliseconds since the UNIX epoch as `YYYY-MM-DD HH:MM UTC`.
pub fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant, `civil_from_days`).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// Renders the list page. `join_hint(app, id)` is the command shown for an endpoint.
pub fn page(entries: &[Entry], join_hint: impl Fn(&str, &str) -> String) -> String {
    let mut out = String::from(PAGE_HEAD);
    if entries.is_empty() {
        out.push_str("<p>No endpoints yet.</p>\n");
    } else {
        out.push_str(
            "<table>\n<tr><th>Label</th><th>App</th><th>Endpoint</th><th>Minted</th><th>Join</th><th>Status</th></tr>\n",
        );
        for e in entries {
            let id = html_escape(&e.id);
            let label = if e.label.is_empty() {
                "<span class=dim>none</span>".to_string()
            } else {
                html_escape(&e.label)
            };
            let app = html_escape(&e.app);
            let hint = html_escape(&join_hint(&e.app, &e.id));
            let _ = writeln!(
                out,
                "<tr><td>{label}</td><td>{app}</td><td><code>{id}</code> <button data-copy=\"{id}\">copy</button></td>\
                 <td>{}</td><td><code>{hint}</code></td>\
                 <td><button data-status=\"{id}\">show</button> <span class=status></span></td></tr>",
                utc(e.minted_at)
            );
        }
        out.push_str("</table>\n");
    }
    out.push_str(PAGE_TAIL);
    out
}

const PAGE_HEAD: &str = "<!doctype html>
<html lang=en><head><meta charset=utf-8><meta name=viewport content=\"width=device-width\">
<title>iroh-do endpoints</title>
<style>
body{font:15px/1.4 system-ui,sans-serif;margin:1.5em;color:#111;background:#fff}
@media (prefers-color-scheme:dark){body{color:#ddd;background:#111}}
table{border-collapse:collapse}td,th{padding:.3em .6em;text-align:left;vertical-align:top}
tr+tr{border-top:1px solid #8884}code{font-size:13px;word-break:break-all}.dim{opacity:.6}
</style></head><body>
<h1>iroh-do endpoints</h1>
<p>Each endpoint is an iroh endpoint hosted in a Cloudflare Durable Object, running one app.
Anyone who knows an endpoint's id can connect to it. Status wakes the endpoint's object.</p>
";

const PAGE_TAIL: &str = "<script>
document.addEventListener('click', async (ev) => {
  const b = ev.target;
  if (b.dataset.copy) { await navigator.clipboard.writeText(b.dataset.copy); b.textContent = 'copied'; }
  if (b.dataset.status) {
    const out = b.nextElementSibling;
    out.textContent = '…';
    try {
      const r = await fetch('/status/' + b.dataset.status);
      const s = await r.json();
      const core = ['id', 'app', 'state', 'wakes', 'endpoint_builds', 'connections', 'bundle', 'since_wake_ms'];
      const extra = Object.entries(s).filter(([k]) => !core.includes(k)).map(([k, v]) => `${k} ${v}`);
      out.textContent = [s.state, `${s.wakes} wakes`, ...extra].join(', ');
    } catch (e) { out.textContent = 'unavailable'; }
  }
});
</script>
</body></html>
";

#[cfg(test)]
mod tests {
    use super::*;

    /// Labels are escaped wherever the page shows them.
    #[test]
    fn page_escapes_labels() {
        let e = Entry {
            id: "ybndrfg8".into(),
            app: "chat".into(),
            label: "<script>alert('x')</script> & \"q\"".into(),
            minted_at: 0,
        };
        let html = page(&[e], |_, id| format!("join {id}"));
        assert!(html.contains("<code>join ybndrfg8</code>"), "{html}");
        assert!(!html.contains("<script>alert"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;q&quot;")
        );
        assert_eq!(
            html.matches("<script>").count(),
            1,
            "only the page's own script"
        );
    }

    #[test]
    fn mint_bodies() {
        let label = |b: &str| mint_body(b).map(|(_, l)| l);
        assert_eq!(mint_body("").unwrap(), (None, String::new()));
        assert_eq!(label("{}").unwrap(), "");
        assert_eq!(label(r#"{"label":" ana's room "}"#).unwrap(), "ana's room");
        assert!(label(&format!(r#"{{"label":"{}"}}"#, "é".repeat(65))).is_err());
        assert!(label(&format!(r#"{{"label":"{}"}}"#, "é".repeat(64))).is_ok());
        assert!(label(r#"{"label":"a\nb"}"#).is_err());
        assert!(label("label=x").is_err());
        assert_eq!(
            mint_body(r#"{"app":"echo"}"#).unwrap(),
            (Some("echo".to_string()), String::new())
        );
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(utc(1_792_613_810_522), "2026-10-21 20:16 UTC");
    }

    /// The list reads the `minted_at` index backwards instead of sorting the table.
    #[test]
    fn list_uses_index() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        for s in SCHEMA {
            db.execute(s, []).unwrap();
        }
        let plan: String = db
            .query_row(&format!("EXPLAIN QUERY PLAN {LIST}"), [1], |r| r.get(3))
            .unwrap();
        assert!(plan.contains("endpoints_minted"), "{plan}");
    }
}
