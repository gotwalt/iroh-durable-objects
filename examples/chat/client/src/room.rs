//! `iroh-do/room/1` messages, input parsing, rendering and per-room client
//! state (design note 008). Nothing here does I/O on the network.

use std::{
    collections::BTreeSet,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use chrono::TimeZone;
use iroh::{EndpointId, SecretKey};
use n0_error::{Result, StdResultExt, anyerr, bail_any};
use serde::{Deserialize, Serialize};

/// Maximum length of one request line in bytes, newline excluded (note 008).
pub const MAX_REQUEST: usize = 1024;

/// A member as listed in a `who` reply.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Member {
    /// EndpointId, z-base-32.
    pub peer: String,
    /// Current nick.
    pub nick: String,
    /// Whether the member's relay socket to the room is open.
    #[serde(default)]
    pub present: bool,
}

/// The `you` object of a `ready` reply.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct You {
    /// Our EndpointId, z-base-32.
    pub peer: String,
    /// Our nick in this room.
    pub nick: String,
}

/// One JSON line from the room: a reply or a delivered event.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Event {
    /// A message.
    Msg {
        /// Log sequence number.
        seq: u64,
        /// `seq` of the room event before this one, 0 if none.
        #[serde(default)]
        prev: u64,
        /// Milliseconds since the UNIX epoch, object clock.
        ts: i64,
        /// Sender, z-base-32.
        peer: String,
        /// Sender's nick at the time.
        nick: String,
        /// Message text.
        text: String,
    },
    /// An action (`/me`).
    Act {
        /// Log sequence number.
        seq: u64,
        /// `seq` of the room event before this one, 0 if none.
        #[serde(default)]
        prev: u64,
        /// Milliseconds since the UNIX epoch, object clock.
        ts: i64,
        /// Sender, z-base-32.
        peer: String,
        /// Sender's nick at the time.
        nick: String,
        /// Action text.
        text: String,
    },
    /// A member joined.
    Join {
        /// Log sequence number.
        seq: u64,
        /// `seq` of the room event before this one, 0 if none.
        #[serde(default)]
        prev: u64,
        /// Milliseconds since the UNIX epoch, object clock.
        ts: i64,
        /// Member, z-base-32.
        peer: String,
        /// Member's nick.
        nick: String,
    },
    /// A member left with `/quit`.
    Part {
        /// Log sequence number.
        seq: u64,
        /// `seq` of the room event before this one, 0 if none.
        #[serde(default)]
        prev: u64,
        /// Milliseconds since the UNIX epoch, object clock.
        ts: i64,
        /// Member, z-base-32.
        peer: String,
        /// Member's nick.
        nick: String,
        /// Reason given to `/quit`, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A member changed nick.
    Nick {
        /// Log sequence number.
        seq: u64,
        /// `seq` of the room event before this one, 0 if none.
        #[serde(default)]
        prev: u64,
        /// Milliseconds since the UNIX epoch, object clock.
        ts: i64,
        /// Member, z-base-32.
        peer: String,
        /// Previous nick.
        old: String,
        /// New nick.
        new: String,
    },
    /// Reply to `/who`.
    Who {
        /// Joined members.
        members: Vec<Member>,
    },
    /// Last line of the reply to `/join`.
    Ready {
        /// The room's EndpointId, z-base-32.
        room: String,
        /// Our membership.
        you: You,
        /// Newest sequence number in the log.
        last_seq: u64,
    },
    /// A request was accepted and stored as event `seq`.
    Ok {
        /// Sequence number of the stored event.
        seq: u64,
        /// `seq` of the room event before it, 0 if none.
        #[serde(default)]
        prev: u64,
    },
    /// A request was refused.
    Err {
        /// Why.
        text: String,
    },
    /// A line type this client does not know.
    #[serde(other)]
    Unknown,
}

impl Event {
    /// Parses one line.
    pub fn parse(line: &str) -> Result<Self> {
        serde_json::from_str(line).std_context("parsing room/1 JSON line")
    }

    /// The sequence number of a stored event; `None` for replies.
    pub fn seq(&self) -> Option<u64> {
        match self {
            Event::Msg { seq, .. }
            | Event::Act { seq, .. }
            | Event::Join { seq, .. }
            | Event::Part { seq, .. }
            | Event::Nick { seq, .. } => Some(*seq),
            _ => None,
        }
    }

    /// The `seq` of the room event before a stored event; `None` for replies.
    pub fn prev(&self) -> Option<u64> {
        match self {
            Event::Msg { prev, .. }
            | Event::Act { prev, .. }
            | Event::Join { prev, .. }
            | Event::Part { prev, .. }
            | Event::Nick { prev, .. } => Some(*prev),
            _ => None,
        }
    }

    /// Renders the event as one IRC-style line, in the time zone `tz`.
    ///
    /// Returns `None` for `ok`, `ready` and unknown lines, which the caller
    /// handles. `today` is the current date in `tz`; events from other days
    /// carry the date.
    pub fn render<Tz: TimeZone>(&self, tz: &Tz, today: chrono::NaiveDate) -> Option<String>
    where
        Tz::Offset: std::fmt::Display,
    {
        let at = |ts: i64| stamp(ts, tz, today);
        let line = match self {
            Event::Msg { ts, nick, text, .. } => {
                format!("{} <{}> {}", at(*ts), clean(nick), clean(text))
            }
            Event::Act { ts, nick, text, .. } => {
                format!("{} * {} {}", at(*ts), clean(nick), clean(text))
            }
            Event::Join { ts, nick, .. } => format!("{} -- {} joined", at(*ts), clean(nick)),
            Event::Part {
                ts, nick, reason, ..
            } => match reason.as_deref().filter(|r| !r.is_empty()) {
                Some(r) => format!("{} -- {} left ({})", at(*ts), clean(nick), clean(r)),
                None => format!("{} -- {} left", at(*ts), clean(nick)),
            },
            Event::Nick { ts, old, new, .. } => {
                format!("{} -- {} is now {}", at(*ts), clean(old), clean(new))
            }
            Event::Who { members } => {
                let list = |present: bool| {
                    members
                        .iter()
                        .filter(|m| m.present == present)
                        .map(|m| clean(&m.nick))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let (here, away) = (list(true), list(false));
                match (here.is_empty(), away.is_empty()) {
                    (_, true) => format!("-- here: {here}"),
                    (true, false) => format!("-- here: (nobody); away: {away}"),
                    (false, false) => format!("-- here: {here}; away: {away}"),
                }
            }
            Event::Err { text } => format!("!! {}", clean(text)),
            Event::Ready { .. } | Event::Ok { .. } | Event::Unknown => return None,
        };
        Some(line)
    }
}

/// `[HH:MM]` for today, `[YYYY-MM-DD HH:MM]` otherwise.
fn stamp<Tz: TimeZone>(ts: i64, tz: &Tz, today: chrono::NaiveDate) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match tz.timestamp_millis_opt(ts).single() {
        Some(t) if t.date_naive() == today => t.format("[%H:%M]").to_string(),
        Some(t) => t.format("[%Y-%m-%d %H:%M]").to_string(),
        None => "[--:--]".to_string(),
    }
}

/// Replaces control characters, so text from other members cannot move the
/// cursor or change terminal state.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// One line typed by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Nothing to send.
    Empty,
    /// Plain text, sent with `/say`.
    Say(String),
    /// `/me <text>`.
    Me(String),
    /// `/nick <name>`.
    Nick(String),
    /// `/who`.
    Who,
    /// `/history [n]`.
    History(Option<u32>),
    /// `/quit [reason]`: leave the room and exit.
    Quit(Option<String>),
    /// `/help`, answered locally.
    Help,
}

/// Help text for the interactive client.
pub const HELP: &str = "\
-- text is sent to the room; //text sends text that starts with /
-- /me <action>   /nick <name>   /who   /history [n]   /help
-- /quit [reason] leaves the room and exits; Esc or Ctrl-C exits and stays a member
-- PgUp, PgDn and the mouse wheel scroll; past the top, older events load";

impl Input {
    /// Parses one typed line. Errors are messages for the user.
    pub fn parse(line: &str) -> std::result::Result<Self, String> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            return Ok(Input::Empty);
        }
        if let Some(rest) = line.strip_prefix("//") {
            return Ok(Input::Say(format!("/{rest}")));
        }
        let Some(cmd) = line.strip_prefix('/') else {
            return Ok(Input::Say(line.to_string()));
        };
        let (name, arg) = match cmd.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (cmd, ""),
        };
        let arg_opt = (!arg.is_empty()).then(|| arg.to_string());
        match name {
            "me" if !arg.is_empty() => Ok(Input::Me(arg.to_string())),
            "me" => Err("usage: /me <action>".into()),
            "nick" if !arg.is_empty() && !arg.contains(char::is_whitespace) => {
                Ok(Input::Nick(arg.to_string()))
            }
            "nick" => Err("usage: /nick <name>".into()),
            "who" => Ok(Input::Who),
            "history" => match arg_opt {
                None => Ok(Input::History(None)),
                Some(n) => n
                    .parse()
                    .map(|n| Input::History(Some(n)))
                    .map_err(|_| "usage: /history [n]".into()),
            },
            "quit" => Ok(Input::Quit(arg_opt)),
            "help" => Ok(Input::Help),
            _ => Err(format!(
                "unknown command /{name}; //{name} sends it as text (/help)"
            )),
        }
    }

    /// The request line sent to the room, if any.
    pub fn request(&self) -> Option<String> {
        match self {
            Input::Empty | Input::Help => None,
            Input::Say(t) => Some(format!("/say {t}")),
            Input::Me(t) => Some(format!("/me {t}")),
            Input::Nick(n) => Some(format!("/nick {n}")),
            Input::Who => Some("/who".into()),
            Input::History(None) => Some("/history".into()),
            Input::History(Some(n)) => Some(format!("/history {n}")),
            Input::Quit(None) => Some("/quit".into()),
            Input::Quit(Some(r)) => Some(format!("/quit {r}")),
        }
    }
}

/// Checks a request line against the protocol limits.
pub fn check_request(line: &str) -> Result<()> {
    if line.contains('\n') {
        bail_any!("request lines must not contain newlines");
    }
    if line.len() > MAX_REQUEST {
        bail_any!(
            "line is {} bytes; the room accepts at most {MAX_REQUEST}",
            line.len()
        );
    }
    Ok(())
}

/// Which events this client has shown, so deliveries, replies to `/since`
/// and our own echoes are each shown once.
#[derive(Debug, Default)]
pub struct Seen {
    seqs: BTreeSet<u64>,
    /// Highest sequence number seen; persisted as the `/since` cursor.
    pub last: u64,
}

impl Seen {
    /// Starts from a persisted cursor.
    pub fn new(last: u64) -> Self {
        Self {
            seqs: BTreeSet::new(),
            last,
        }
    }

    /// Records `seq`; returns `false` if it was already seen.
    pub fn insert(&mut self, seq: u64) -> bool {
        if !self.seqs.insert(seq) {
            return false;
        }
        self.last = self.last.max(seq);
        true
    }

    /// Whether an event whose previous room event is `prev` follows room events
    /// this client has not seen.
    ///
    /// `last` is the newest room event seen, so events were missed exactly when
    /// `prev > last`; `/since last` returns them. Diagnostic records between two
    /// room events do not count, because `prev` skips them.
    pub fn missed(&self, prev: u64) -> bool {
        prev > self.last
    }
}

/// Nick used when `--nick` is absent: `$USER` if it is a valid nick, else `me`.
pub fn default_nick() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| check_nick(u).is_ok())
        .unwrap_or_else(|| "me".to_string())
}

/// Checks the room's nick rule (1–24 characters of `[A-Za-z0-9_.-]`, note 008).
///
/// `.` and `..` are also refused, because the nick names a state directory.
pub fn check_nick(nick: &str) -> Result<()> {
    let ok = (1..=24).contains(&nick.len())
        && nick
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && nick != "."
        && nick != "..";
    if !ok {
        bail_any!("invalid nick {nick:?}: use 1-24 characters of A-Z a-z 0-9 _ . -");
    }
    Ok(())
}

/// Per-member client state on disk: the member's secret key and the `/since`
/// cursor. The key makes nick and membership survive client restarts.
///
/// One directory per room and nick, so several members of one room can run
/// on the same machine.
#[derive(Debug, Clone)]
pub struct State {
    dir: PathBuf,
    nick: String,
}

impl State {
    /// State for member `nick` of room `room` under `base`, or under the
    /// user's data directory (`$XDG_DATA_HOME`, `~/.local/share`,
    /// `~/Library/Application Support`).
    ///
    /// The directory is `rooms/<E>/<nick>`, with the nick in lowercase: the
    /// room compares nicks ignoring ASCII case, and so do the file systems
    /// macOS uses by default.
    pub fn new(base: Option<&Path>, room: EndpointId, nick: &str) -> Result<Self> {
        check_nick(nick)?;
        let base = match base {
            Some(b) => b.to_path_buf(),
            None => dirs::data_dir()
                .ok_or_else(|| anyerr!("no user data directory; pass --state-dir"))?
                // The binary was called iroh-do-demo when this path was chosen;
                // it stays, so existing member keys keep working.
                .join("iroh-do-demo"),
        };
        let dir = base
            .join("rooms")
            .join(room.to_z32())
            .join(nick.to_ascii_lowercase());
        create_private_dir(&dir)?;
        Ok(Self {
            dir,
            nick: nick.to_string(),
        })
    }

    /// Takes an exclusive lock on this member's state for the life of the
    /// returned file.
    ///
    /// Two clients with the same key would both open a relay socket for the
    /// same EndpointId, and the ingress keeps only the newest one, so they
    /// would take turns being unreachable.
    pub fn lock(&self) -> Result<fs::File> {
        let path = self.dir.join("lock");
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_std_context(|_| format!("opening {}", path.display()))?;
        match f.try_lock() {
            Ok(()) => Ok(f),
            Err(fs::TryLockError::WouldBlock) => Err(anyerr!(
                "another client is already running as {} in this room (state {}); \
                 pick another --nick to join as a second member",
                self.nick,
                self.dir.display()
            )),
            Err(fs::TryLockError::Error(err)) => {
                Err(err).with_std_context(|_| format!("locking {}", path.display()))
            }
        }
    }

    /// Whether this member has a key yet, that is, has started before.
    pub fn has_key(&self) -> bool {
        self.dir.join("secret_key").exists()
    }

    /// The directory holding this member's files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Loads the secret key, creating it (mode 0600) on first use.
    pub fn secret_key(&self) -> Result<SecretKey> {
        let path = self.dir.join("secret_key");
        match fs::read(&path) {
            Ok(bytes) => {
                let bytes: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyerr!("{} is not 32 bytes", path.display()))?;
                Ok(SecretKey::from_bytes(&bytes))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let key = SecretKey::generate();
                let mut opts = fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
                let mut f = opts
                    .open(&path)
                    .with_std_context(|_| format!("creating {}", path.display()))?;
                f.write_all(&key.to_bytes())
                    .with_std_context(|_| format!("writing {}", path.display()))?;
                Ok(key)
            }
            Err(err) => Err(err).with_std_context(|_| format!("reading {}", path.display())),
        }
    }

    /// The persisted `/since` cursor, 0 if none.
    pub fn last_seq(&self) -> u64 {
        fs::read_to_string(self.dir.join("last_seq"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Persists the `/since` cursor (write to a temporary file, then rename).
    pub fn set_last_seq(&self, seq: u64) -> Result<()> {
        let tmp = self.dir.join("last_seq.tmp");
        fs::write(&tmp, format!("{seq}\n"))
            .with_std_context(|_| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, self.dir.join("last_seq")).std_context("renaming last_seq")?;
        Ok(())
    }
}

fn create_private_dir(dir: &Path) -> Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
        .with_std_context(|_| format!("creating {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    const P: &str = "ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u";
    // 2026-10-21T09:05:00Z
    const TS: i64 = 1_792_573_500_000;

    fn today() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 21).unwrap()
    }

    fn render(line: &str) -> Option<String> {
        Event::parse(line).unwrap().render(&Utc, today())
    }

    #[test]
    fn renders_events_irc_style() {
        let msg =
            format!(r#"{{"t":"msg","seq":12,"ts":{TS},"peer":"{P}","nick":"ana","text":"hi"}}"#);
        assert_eq!(render(&msg).unwrap(), "[09:05] <ana> hi");
        let act =
            format!(r#"{{"t":"act","seq":13,"ts":{TS},"peer":"{P}","nick":"ana","text":"waves"}}"#);
        assert_eq!(render(&act).unwrap(), "[09:05] * ana waves");
        let join =
            format!(r#"{{"t":"join","seq":14,"ts":{TS},"peer":"{P}","nick":"bob","hist":true}}"#);
        assert_eq!(render(&join).unwrap(), "[09:05] -- bob joined");
        let part = format!(
            r#"{{"t":"part","seq":15,"ts":{TS},"peer":"{P}","nick":"bob","reason":"bye"}}"#
        );
        assert_eq!(render(&part).unwrap(), "[09:05] -- bob left (bye)");
        let part = format!(r#"{{"t":"part","seq":15,"ts":{TS},"peer":"{P}","nick":"bob"}}"#);
        assert_eq!(render(&part).unwrap(), "[09:05] -- bob left");
        let nick =
            format!(r#"{{"t":"nick","seq":16,"ts":{TS},"peer":"{P}","old":"bo","new":"bob"}}"#);
        assert_eq!(render(&nick).unwrap(), "[09:05] -- bo is now bob");
        let old = format!(
            r#"{{"t":"msg","seq":1,"ts":{},"peer":"{P}","nick":"ana","text":"x"}}"#,
            TS - 86_400_000
        );
        assert_eq!(render(&old).unwrap(), "[2026-10-20 09:05] <ana> x");
    }

    #[test]
    fn renders_replies() {
        let who = r#"{"t":"who","members":[{"peer":"a","nick":"ana","present":true},{"peer":"b","nick":"bob","present":false},{"peer":"c","nick":"cy","present":true}]}"#;
        assert_eq!(render(who).unwrap(), "-- here: ana, cy; away: bob");
        assert_eq!(
            render(r#"{"t":"err","text":"rate limit"}"#).unwrap(),
            "!! rate limit"
        );
        assert_eq!(render(r#"{"t":"ok","seq":17}"#), None);
        assert_eq!(
            Event::parse(r#"{"t":"ok","seq":17}"#).unwrap(),
            Event::Ok { seq: 17, prev: 0 }
        );
        assert_eq!(
            Event::parse(r#"{"t":"later","x":1}"#).unwrap(),
            Event::Unknown
        );
        let ready = Event::parse(&format!(
            r#"{{"t":"ready","room":"{P}","you":{{"peer":"{P}","nick":"ana"}},"last_seq":16}}"#
        ))
        .unwrap();
        assert!(matches!(ready, Event::Ready { last_seq: 16, .. }));
    }

    #[test]
    fn strips_control_characters() {
        let msg = format!(
            r#"{{"t":"msg","seq":1,"ts":{TS},"peer":"{P}","nick":"m\u001b[2J","text":"a\rb"}}"#
        );
        assert_eq!(render(&msg).unwrap(), "[09:05] <m\u{fffd}[2J> a\u{fffd}b");
    }

    #[test]
    fn parses_input() {
        use Input::*;
        assert_eq!(Input::parse("  ").unwrap(), Empty);
        assert_eq!(
            Input::parse("hello /there").unwrap(),
            Say("hello /there".into())
        );
        assert_eq!(Input::parse("//who").unwrap(), Say("/who".into()));
        assert_eq!(Input::parse("/me waves").unwrap(), Me("waves".into()));
        assert!(Input::parse("/me").is_err());
        assert_eq!(Input::parse("/nick bob").unwrap(), Nick("bob".into()));
        assert!(Input::parse("/nick a b").is_err());
        assert_eq!(Input::parse("/who").unwrap(), Who);
        assert_eq!(Input::parse("/history").unwrap(), History(None));
        assert_eq!(Input::parse("/history 20").unwrap(), History(Some(20)));
        assert!(Input::parse("/history x").is_err());
        assert_eq!(Input::parse("/quit").unwrap(), Quit(None));
        assert_eq!(
            Input::parse("/quit gone home").unwrap(),
            Quit(Some("gone home".into()))
        );
        assert!(Input::parse("/frob").is_err());
        assert_eq!(Say("/x".into()).request().unwrap(), "/say /x");
        assert_eq!(Quit(Some("bye".into())).request().unwrap(), "/quit bye");
        assert_eq!(Help.request(), None);
        assert!(check_request(&"x".repeat(MAX_REQUEST)).is_ok());
        assert!(check_request(&"x".repeat(MAX_REQUEST + 1)).is_err());
    }

    #[test]
    fn seen_dedups_and_detects_missed_events() {
        let mut s = Seen::new(10);
        // ctl records 11 and 12 sit between room events 10 and 13: no gap.
        assert!(!s.missed(10));
        // Room event 12 came after 10 and was not seen.
        assert!(s.missed(12));
        assert!(s.insert(11));
        assert!(!s.insert(11));
        assert!(s.insert(9));
        assert_eq!(s.last, 11);
        assert!(!s.missed(11));
        assert!(Seen::new(0).missed(40));
        assert!(!Seen::new(0).missed(0));
    }

    #[test]
    fn state_persists_key_and_cursor() {
        let base = std::env::temp_dir().join(format!("iroh-do-demo-test-{}", std::process::id()));
        let room = SecretKey::generate().public();
        let st = State::new(Some(&base), room, "ana").unwrap();
        assert!(!st.has_key());
        let k1 = st.secret_key().unwrap();
        assert!(st.has_key());
        let k2 = State::new(Some(&base), room, "Ana")
            .unwrap()
            .secret_key()
            .unwrap();
        assert_eq!(k1.public(), k2.public());
        let lock = st.lock().unwrap();
        let err = st.lock().unwrap_err().to_string();
        assert!(err.contains("running as ana"), "{err}");
        assert!(err.contains("pick another --nick"), "{err}");
        drop(lock);
        assert!(st.lock().is_ok());
        assert_eq!(st.last_seq(), 0);
        st.set_last_seq(42).unwrap();
        assert_eq!(st.last_seq(), 42);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(st.dir().join("secret_key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn state_is_per_room_and_nick() {
        let base =
            std::env::temp_dir().join(format!("iroh-do-demo-test-nick-{}", std::process::id()));
        let room = SecretKey::generate().public();
        let ana = State::new(Some(&base), room, "ana").unwrap();
        let bo = State::new(Some(&base), room, "bo").unwrap();
        assert_eq!(
            ana.dir(),
            base.join("rooms").join(room.to_z32()).join("ana")
        );
        assert_ne!(ana.dir(), bo.dir());
        assert_ne!(
            ana.secret_key().unwrap().public(),
            bo.secret_key().unwrap().public()
        );
        // Both members can run at once.
        let _a = ana.lock().unwrap();
        let _b = bo.lock().unwrap();
        // State from before per-nick directories sits beside them and is ignored.
        fs::write(
            base.join("rooms").join(room.to_z32()).join("secret_key"),
            [0; 32],
        )
        .unwrap();
        assert!(State::new(Some(&base), room, "cy").is_ok());
        for bad in ["", ".", "..", "a/b", "a b", &"x".repeat(25)] {
            assert!(State::new(Some(&base), room, bad).is_err(), "{bad:?}");
        }
        fs::remove_dir_all(&base).unwrap();
    }
}
