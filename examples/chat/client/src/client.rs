//! The chat room client: connections, requests, deliveries and catch-up
//! (design note 008). It runs as one task, takes [`Command`]s and emits
//! [`Update`]s, so the terminal UI and the scripted modes share it.
//!
//! The client never contacts the room on a timer. It sends a request when the
//! user acts, when a delivery suggests a gap, when its home relay connection
//! (its relay socket to the room) comes back after a drop, and to refresh the
//! member list after a join, part or nick event.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, EndpointId, Watcher,
    endpoint::{Connection, RecvStream},
};
use n0_error::{Result, StdResultExt, anyerr, bail_any};
use tokio::sync::mpsc;

use crate::{
    net::{ROOM_ALPN, Target, bind_room_endpoint, connect},
    room::{Event, Input, Member, Seen, State, check_request},
};

/// How long to wait for the reply to one request.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest reply or delivery stream read; `/since` returns up to 500 events.
const MAX_STREAM: usize = 2 * 1024 * 1024;
/// Events produced together (a join followed by a nick change) cause one `/who`.
const WHO_DELAY: Duration = Duration::from_millis(300);
/// Most events `/history` returns (note 008).
pub const MAX_HISTORY: usize = 200;

/// Where an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A reply to `/join`, `/since` or `/history`.
    Hist,
    /// Delivered by the room as it happened.
    Live,
    /// Our own request, built locally from the room's `ok`.
    Own,
}

/// Connection state shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    /// Before the reply to `/join`.
    Connecting,
    /// Joined; deliveries arrive while the home relay connection is up.
    Joined,
    /// Catching up after the home relay connection came back or a request failed.
    Reconnecting,
}

/// Output of the client.
#[derive(Debug, Clone)]
pub enum Update {
    /// An event or reply to show. `raw` is the JSON line as received, or as
    /// built for [`Origin::Own`].
    Event {
        /// The parsed line.
        ev: Event,
        /// The JSON line.
        raw: String,
        /// Where it came from.
        origin: Origin,
    },
    /// Older events for scrollback, oldest first. Empty when there are none.
    Older(Vec<Event>),
    /// The current member list.
    Members(Vec<Member>),
    /// The `/join` reply completed.
    Joined {
        /// Our nick.
        nick: String,
    },
    /// Our nick changed.
    Nick(String),
    /// Connection state changed.
    Conn(ConnState),
    /// Whether the home relay connection (to the room) is up.
    Relay(bool),
    /// A local error, such as a failed request.
    Error(String),
    /// `/quit` succeeded; the member left the room.
    Left,
}

/// Input to the client.
#[derive(Debug, Clone)]
pub enum Command {
    /// A typed line other than `/help`.
    Send(Input),
    /// Fetch events older than `before` for scrollback; `have` events are loaded.
    Older {
        /// Number of events already loaded.
        have: usize,
        /// Oldest loaded sequence number.
        before: u64,
    },
}

/// Result of [`Client::send`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// The reply was shown.
    Done,
    /// The room refused the request; the `err` line was shown.
    Refused(String),
    /// `/quit` succeeded; the member left the room.
    Left,
}

/// Options for [`Client::start`].
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Nick to join with; it also selects the member's state directory.
    /// `None` uses [`crate::room::default_nick`] and keeps a returning member's nick.
    pub nick: Option<String>,
    /// Base directory for per-member state instead of the user's data directory.
    pub state_dir: Option<PathBuf>,
}

/// The connection to the room, either dialed or accepted.
#[derive(Clone)]
struct Room {
    ep: Endpoint,
    target: Target,
    id: EndpointId,
    conn: Arc<Mutex<Option<Connection>>>,
    deliveries: mpsc::UnboundedSender<String>,
}

impl Room {
    /// The open connection to the room, if any.
    fn current(&self) -> Option<Connection> {
        let guard = self.conn.lock().expect("poisoned");
        guard
            .as_ref()
            .filter(|c| c.close_reason().is_none())
            .cloned()
    }

    /// Makes `conn` the connection for requests and reads its deliveries.
    fn adopt(&self, conn: Connection) {
        *self.conn.lock().expect("poisoned") = Some(conn.clone());
        let tx = self.deliveries.clone();
        tokio::spawn(async move {
            while let Ok(recv) = conn.accept_uni().await {
                let tx = tx.clone();
                tokio::spawn(read_lines(recv, tx));
            }
        });
    }

    /// Accepts connections from the room; closes connections from anyone else.
    fn spawn_acceptor(&self) {
        let room = self.clone();
        tokio::spawn(async move {
            while let Some(incoming) = room.ep.accept().await {
                let room = room.clone();
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    if conn.alpn() != ROOM_ALPN || conn.remote_id() != room.id {
                        conn.close(1u32.into(), b"not the room");
                        return;
                    }
                    room.adopt(conn);
                });
            }
        });
    }

    /// Sends one request and returns the reply lines.
    ///
    /// Reuses an open connection in either direction before dialing (note
    /// 008). A request is retried on a new connection only if it failed before
    /// it was sent, so the room never sees it twice.
    async fn request(&self, line: &str) -> Result<Vec<String>> {
        check_request(line)?;
        if let Some(conn) = self.current() {
            match exchange(&conn, line).await {
                Ok(lines) => return Ok(lines),
                Err(Failure::NotSent(_)) => {}
                Err(Failure::Sent(err)) => return Err(err),
            }
        }
        let conn = connect(&self.ep, &self.target, ROOM_ALPN).await?.conn;
        self.adopt(conn.clone());
        exchange(&conn, line).await.map_err(Failure::into_inner)
    }
}

enum Failure {
    NotSent(n0_error::AnyError),
    Sent(n0_error::AnyError),
}

impl Failure {
    fn into_inner(self) -> n0_error::AnyError {
        match self {
            Failure::NotSent(e) | Failure::Sent(e) => e,
        }
    }
}

/// One request on its own bidirectional stream.
async fn exchange(conn: &Connection, line: &str) -> std::result::Result<Vec<String>, Failure> {
    let (mut send, mut recv) = conn.open_bi().await.anyerr().map_err(Failure::NotSent)?;
    send.write_all(format!("{line}\n").as_bytes())
        .await
        .anyerr()
        .map_err(Failure::NotSent)?;
    send.finish().anyerr().map_err(Failure::NotSent)?;
    let data = tokio::time::timeout(REPLY_TIMEOUT, recv.read_to_end(MAX_STREAM))
        .await
        .map_err(|_| Failure::Sent(anyerr!("no reply within {REPLY_TIMEOUT:?}")))?
        .anyerr()
        .map_err(Failure::Sent)?;
    Ok(split_lines(&data))
}

async fn read_lines(mut recv: RecvStream, tx: mpsc::UnboundedSender<String>) {
    if let Ok(data) = recv.read_to_end(MAX_STREAM).await {
        for line in split_lines(&data) {
            let _ = tx.send(line);
        }
    }
}

fn split_lines(data: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(data)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// A joined member of one room.
pub struct Client {
    ep: Endpoint,
    room: Room,
    deliveries: mpsc::UnboundedReceiver<String>,
    state: State,
    seen: Seen,
    me: String,
    nick: String,
    updates: mpsc::UnboundedSender<Update>,
    who_due: Option<Instant>,
    _lock: std::fs::File,
}

impl Client {
    /// Binds the member endpoint, dials the room and joins.
    ///
    /// Emits the history, then [`Update::Joined`], then any events since the
    /// persisted cursor.
    pub async fn start(
        target: &Target,
        opts: &Options,
        updates: mpsc::UnboundedSender<Update>,
    ) -> Result<Self> {
        let relay = target.relay_url().await?;
        let nick = opts.nick.clone().unwrap_or_else(crate::room::default_nick);
        let state = State::new(opts.state_dir.as_deref(), target.id, &nick)?;
        let lock = state.lock()?;
        // A returning member joined without --nick keeps the nick the room
        // has for it, which /nick may have changed. A new one asks for the
        // nick its state is named after.
        let join_nick = (opts.nick.is_some() || !state.has_key()).then_some(nick);
        let ep = bind_room_endpoint(state.secret_key()?, relay.clone()).await?;
        let (tx, deliveries) = mpsc::unbounded_channel();
        let room = Room {
            ep: ep.clone(),
            target: Target {
                id: target.id,
                relay_url: Some(relay),
            },
            id: target.id,
            conn: Default::default(),
            deliveries: tx,
        };
        room.spawn_acceptor();
        let mut client = Self {
            seen: Seen::new(state.last_seq()),
            me: ep.id().to_z32(),
            ep,
            room,
            deliveries,
            state,
            nick: String::new(),
            updates,
            who_due: None,
            _lock: lock,
        };
        client.emit(Update::Conn(ConnState::Connecting));
        // No need to wait for `Endpoint::online`: dialing the room opens the
        // relay socket to the room, which is also the home relay connection.
        client.join(join_nick.as_deref()).await?;
        Ok(client)
    }

    /// The member endpoint. Closing it ends presence in the room.
    pub fn endpoint(&self) -> &Endpoint {
        &self.ep
    }

    /// Where this member's key and cursor are stored.
    pub fn state_dir(&self) -> &std::path::Path {
        self.state.dir()
    }

    fn emit(&self, u: Update) {
        let _ = self.updates.send(u);
    }

    async fn join(&mut self, nick: Option<&str>) -> Result<()> {
        let line = match nick {
            Some(n) => format!("/join {n}"),
            None => "/join".to_string(),
        };
        let cursor = self.seen.last;
        let mut events = Vec::new();
        for raw in self.room.request(&line).await? {
            match Event::parse(&raw) {
                Ok(Event::Ready { you, .. }) => self.nick = you.nick,
                Ok(Event::Err { text }) => bail_any!("the room refused /join: {text}"),
                Ok(ev) if ev.seq().is_some() => events.push((ev, raw)),
                _ => {}
            }
        }
        if self.nick.is_empty() {
            bail_any!("the room's reply to /join has no ready line");
        }
        // History is the newest 50 events; /since fills in what lies between
        // the cursor and the oldest of them.
        if cursor > 0 && events.first().and_then(|(e, _)| e.prev()) > Some(cursor) {
            for raw in self.room.request(&format!("/since {cursor}")).await? {
                if let Ok(ev) = Event::parse(&raw)
                    && ev.seq().is_some()
                {
                    events.push((ev, raw));
                }
            }
            events.sort_by_key(|(e, _)| e.seq());
        }
        for (ev, raw) in events {
            self.show(ev, raw, Origin::Hist);
        }
        self.emit(Update::Joined {
            nick: self.nick.clone(),
        });
        self.emit(Update::Conn(ConnState::Joined));
        self.refresh_members().await;
        Ok(())
    }

    /// Emits an event unless it was shown before; advances the cursor.
    fn show(&mut self, ev: Event, raw: String, origin: Origin) {
        if let Some(seq) = ev.seq() {
            if !self.seen.insert(seq) {
                return;
            }
            if let Err(err) = self.state.set_last_seq(self.seen.last) {
                self.emit(Update::Error(format!("{err:#}")));
            }
            if matches!(
                ev,
                Event::Join { .. } | Event::Part { .. } | Event::Nick { .. }
            ) {
                self.who_due.get_or_insert(Instant::now() + WHO_DELAY);
            }
        }
        self.emit(Update::Event { ev, raw, origin });
    }

    /// Requests everything after the cursor and shows events before `until`.
    async fn catch_up(&mut self, until: Option<u64>) -> Result<()> {
        let lines = self
            .room
            .request(&format!("/since {}", self.seen.last))
            .await?;
        for raw in lines {
            match Event::parse(&raw) {
                Ok(ev @ Event::Err { .. }) => self.emit(Update::Event {
                    ev,
                    raw,
                    origin: Origin::Hist,
                }),
                Ok(ev) if ev.seq().is_some_and(|s| s < until.unwrap_or(u64::MAX)) => {
                    self.show(ev, raw, Origin::Hist)
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn refresh_members(&mut self) {
        self.who_due = None;
        match self.room.request("/who").await {
            Ok(lines) => {
                for raw in lines {
                    if let Ok(Event::Who { members }) = Event::parse(&raw) {
                        self.emit(Update::Members(members));
                    }
                }
            }
            Err(err) => self.emit(Update::Error(format!("/who: {err:#}"))),
        }
    }

    /// Handles one line delivered by the room.
    async fn delivery(&mut self, raw: String) {
        let Ok(ev) = Event::parse(&raw) else { return };
        if let (Some(seq), Some(prev)) = (ev.seq(), ev.prev())
            && self.seen.missed(prev)
            && let Err(err) = self.catch_up(Some(seq)).await
        {
            self.emit(Update::Error(format!("catching up: {err:#}")));
        }
        self.show(ev, raw, Origin::Live);
    }

    /// Sends a typed line and shows the reply.
    pub async fn send(&mut self, input: Input) -> Result<Sent> {
        let Some(line) = input.request() else {
            return Ok(Sent::Done);
        };
        let lines = self.room.request(&line).await?;
        let mut sent = Sent::Done;
        for raw in lines {
            let ev = match Event::parse(&raw) {
                Ok(ev) => ev,
                Err(_) => continue,
            };
            match ev {
                Event::Ok { seq, prev } => {
                    if self.seen.missed(prev) {
                        self.catch_up(Some(seq)).await?;
                    }
                    if let Some(own) = self.own_event(&input, seq, prev) {
                        if let Event::Nick { new, .. } = &own {
                            self.nick = new.clone();
                            self.emit(Update::Nick(new.clone()));
                        }
                        if matches!(own, Event::Part { .. }) {
                            sent = Sent::Left;
                        }
                        let raw = serde_json::to_string(&own).unwrap_or_default();
                        self.show(own, raw, Origin::Own);
                    }
                }
                Event::Who { ref members } => {
                    self.emit(Update::Members(members.clone()));
                    self.emit(Update::Event {
                        ev,
                        raw,
                        origin: Origin::Hist,
                    });
                }
                // /history is shown even where it repeats what is on screen.
                ev if ev.seq().is_some() => {
                    if let Some(seq) = ev.seq() {
                        self.seen.insert(seq);
                    }
                    self.emit(Update::Event {
                        ev,
                        raw,
                        origin: Origin::Hist,
                    });
                }
                ev => {
                    if let Event::Err { text } = &ev {
                        sent = Sent::Refused(text.clone());
                    }
                    self.emit(Update::Event {
                        ev,
                        raw,
                        origin: Origin::Hist,
                    })
                }
            }
        }
        Ok(sent)
    }

    /// The event the room stored for our request, which it does not deliver
    /// back to us.
    fn own_event(&self, input: &Input, seq: u64, prev: u64) -> Option<Event> {
        let ts = chrono::Utc::now().timestamp_millis();
        let (peer, nick) = (self.me.clone(), self.nick.clone());
        Some(match input {
            Input::Say(text) => Event::Msg {
                seq,
                prev,
                ts,
                peer,
                nick,
                text: text.clone(),
            },
            Input::Me(text) => Event::Act {
                seq,
                prev,
                ts,
                peer,
                nick,
                text: text.clone(),
            },
            Input::Nick(new) => Event::Nick {
                seq,
                prev,
                ts,
                peer,
                old: nick,
                new: new.clone(),
            },
            Input::Quit(reason) => Event::Part {
                seq,
                prev,
                ts,
                peer,
                nick,
                reason: reason.clone(),
            },
            _ => return None,
        })
    }

    /// Fetches events older than `before` with `/history`.
    async fn older(&mut self, have: usize, before: u64) -> Result<()> {
        let n = (have + 50).min(MAX_HISTORY);
        let mut events = Vec::new();
        for raw in self.room.request(&format!("/history {n}")).await? {
            if let Ok(ev) = Event::parse(&raw)
                && ev.seq().is_some_and(|s| s < before)
            {
                events.push(ev);
            }
        }
        self.emit(Update::Older(events));
        Ok(())
    }

    /// Runs until `commands` closes or the member leaves with `/quit`.
    pub async fn run(&mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        let mut relay = self.ep.home_relay_status();
        let mut relay_up = relay.get().iter().any(|s| s.is_connected());
        self.emit(Update::Relay(relay_up));
        loop {
            let who_due = self.who_due;
            let who = async move {
                match who_due {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                cmd = commands.recv() => {
                    let Some(cmd) = cmd else { return };
                    let res = match cmd {
                        Command::Send(input) => match self.send(input).await {
                            Ok(Sent::Left) => {
                                self.emit(Update::Left);
                                return;
                            }
                            Ok(_) => Ok(()),
                            Err(err) => Err(err),
                        },
                        Command::Older { have, before } => self.older(have, before).await,
                    };
                    if let Err(err) = res {
                        self.emit(Update::Error(format!("{err:#}")));
                    }
                }
                Some(raw) = self.deliveries.recv() => self.delivery(raw).await,
                status = relay.updated() => {
                    let Ok(status) = status else { continue };
                    let up = status.iter().any(|s| s.is_connected());
                    if up == relay_up { continue; }
                    relay_up = up;
                    self.emit(Update::Relay(up));
                    // While the socket was down the room could not deliver.
                    if up {
                        self.emit(Update::Conn(ConnState::Reconnecting));
                        if let Err(err) = self.catch_up(None).await {
                            self.emit(Update::Error(format!("catching up: {err:#}")));
                        }
                        self.emit(Update::Conn(ConnState::Joined));
                    }
                }
                _ = who => self.refresh_members().await,
            }
        }
    }

    /// Handles the next delivered line, waiting up to `dur`. Returns `false`
    /// if none arrived.
    pub async fn next_delivery(&mut self, dur: Duration) -> bool {
        match tokio::time::timeout(dur, self.deliveries.recv()).await {
            Ok(Some(raw)) => {
                self.delivery(raw).await;
                true
            }
            _ => false,
        }
    }

    /// Reads deliveries until `dur` elapses.
    pub async fn listen(&mut self, dur: Duration) {
        let deadline = tokio::time::Instant::now() + dur;
        while let Ok(Some(raw)) = tokio::time::timeout_at(deadline, self.deliveries.recv()).await {
            self.delivery(raw).await;
        }
    }
}
