//! TUI state machine and the background jobs that feed it.

use crate::files::{self, TorrentFile};
use crate::magnet;
use crate::player::{self, Playing};
use crate::proxy::ProxyHandle;
use crate::store;
use crate::torbox::Torbox;
use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// How long the fast poll runs before backing off, and the two intervals.
const FAST_POLL: Duration = Duration::from_secs(2);
const SLOW_POLL: Duration = Duration::from_secs(10);
const FAST_POLL_WINDOW: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum Job {
    /// `checkcached` came back. `cached` false means the file list is empty and
    /// the user must be asked before spending uncached quota.
    Checked { hash: String, uri: String, name: String, files: Vec<TorrentFile>, cached: bool },
    /// The torrent exists in the account and its files carry usable ids.
    Ready { hash: String, torrent_id: i64, files: Vec<TorrentFile> },
    /// Progress of an uncached fetch, shown in the status line.
    Progress { hash: String, progress: f64, state: String },
    Failed { message: String },
}

pub enum Input {
    Key(KeyEvent),
    Job(Job),
    Tick,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Mode {
    /// Typing or pasting a magnet.
    Entry,
    /// Browsing the file list.
    Browse,
    /// "not cached — fetch it?" prompt.
    ConfirmFetch,
}

pub struct Session {
    pub name: String,
    pub uri: String,
    pub torrent_id: Option<i64>,
    /// Display list, video-filtered.
    pub files: Vec<TorrentFile>,
    /// False when the video filter was empty and we fell back to showing all.
    pub filtered: bool,
    pub ready: bool,
    /// Cursor, held per session so switching away and back keeps your place.
    pub list: ListState,
}

impl Session {
    fn new(name: String, uri: String, files: Vec<TorrentFile>, filtered: bool) -> Self {
        Self {
            name,
            uri,
            torrent_id: None,
            files,
            filtered,
            ready: false,
            list: ListState::default().with_selected(Some(0)),
        }
    }

    /// Keeps the cursor inside the list after the file set changes.
    fn clamp_selection(&mut self) {
        if self.files.is_empty() {
            self.list.select(None);
            return;
        }
        let index = self.list.selected().unwrap_or(0).min(self.files.len() - 1);
        self.list.select(Some(index));
    }

    /// One line describing where this session stands, for the status bar.
    fn summary(&self, fetching: Option<f64>) -> String {
        if let Some(progress) = fetching {
            return format!("{} — fetching, {:.0}%", self.name, progress * 100.0);
        }
        if self.filtered {
            format!("{} — {} video file(s)", self.name, self.files.len())
        } else {
            format!("{} — no video files matched, showing everything", self.name)
        }
    }
}

pub struct Pending {
    pub name: String,
    pub progress: f64,
    pub state: String,
}

pub struct App {
    pub mode: Mode,
    pub input: String,
    pub status: String,
    pub error: Option<String>,
    pub sessions: HashMap<String, Session>,
    /// Hashes in the order their sessions first appeared. `sessions` is keyed
    /// for lookup; this is what gives tab-switching a stable, predictable ring.
    pub order: Vec<String>,
    pub current: Option<String>,
    pub pending: HashMap<String, Pending>,
    pub playing: Vec<Playing>,
    /// A file the user picked before the torrent had ids; launched on Ready.
    queued_play: HashMap<String, String>,
    /// Set by the first `q` while streams are live; a second `q` quits.
    pub quit_armed: bool,
    pub should_quit: bool,
    torbox: Torbox,
    proxy: ProxyHandle,
    tx: UnboundedSender<Job>,
    /// Where the session list lives. None disables persistence entirely,
    /// which is what the tests use to keep off the real file.
    store_path: Option<PathBuf>,
}

impl App {
    pub fn new(torbox: Torbox, proxy: ProxyHandle, tx: UnboundedSender<Job>) -> Self {
        Self {
            mode: Mode::Entry,
            input: String::new(),
            status: "paste a magnet link and press Enter".into(),
            error: None,
            sessions: HashMap::new(),
            order: Vec::new(),
            current: None,
            pending: HashMap::new(),
            playing: Vec::new(),
            queued_play: HashMap::new(),
            quit_armed: false,
            should_quit: false,
            torbox,
            proxy,
            tx,
            store_path: store::path().ok(),
        }
    }

    /// Brings back the sessions from the last run. Their torrent and file ids
    /// are kept, so a restored tab is playable without another API round trip.
    pub fn restore(&mut self) {
        let Some(path) = self.store_path.clone() else { return };
        let stored = store::load(&path);
        for saved in stored.sessions.into_iter().take(store::MAX_SESSIONS) {
            let mut session = Session::new(saved.name, saved.uri, saved.files, saved.filtered);
            session.torrent_id = saved.torrent_id;
            session.ready = saved.ready;
            session.list.select(Some(saved.selected));
            session.clamp_selection();
            self.order.push(saved.hash.clone());
            self.sessions.insert(saved.hash, session);
        }
        let focus = stored
            .current
            .filter(|hash| self.sessions.contains_key(hash))
            .or_else(|| self.order.first().cloned());
        if let Some(hash) = focus {
            self.focus(hash);
        }
    }

    /// Writes the session list out. Called whenever the set or the focus
    /// changes — never on fetch progress, which ticks every two seconds.
    fn persist(&mut self) {
        let sessions = self
            .order
            .iter()
            .rev()
            .take(store::MAX_SESSIONS)
            .filter_map(|hash| {
                let session = self.sessions.get(hash)?;
                Some(store::StoredSession {
                    hash: hash.clone(),
                    uri: session.uri.clone(),
                    name: session.name.clone(),
                    torrent_id: session.torrent_id,
                    files: session.files.clone(),
                    filtered: session.filtered,
                    ready: session.ready,
                    selected: session.list.selected().unwrap_or(0),
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let Some(path) = self.store_path.clone() else { return };
        let stored = store::Stored::new(self.current.clone(), sessions);
        if let Err(err) = store::save(&path, &stored) {
            self.error = Some(format!("cannot save the session list: {err}"));
        }
    }

    pub fn session(&self) -> Option<&Session> {
        self.current.as_ref().and_then(|hash| self.sessions.get(hash))
    }

    fn session_mut(&mut self) -> Option<&mut Session> {
        let hash = self.current.clone()?;
        self.sessions.get_mut(&hash)
    }

    /// Moves focus `delta` sessions along the ring, wrapping at both ends.
    fn switch(&mut self, delta: isize) {
        if self.order.is_empty() {
            return;
        }
        // Say why nothing moved, rather than swallowing the key.
        if self.order.len() == 1 {
            self.status = "only one session — press n to open another magnet".into();
            return;
        }
        let position = self
            .current
            .as_ref()
            .and_then(|hash| self.order.iter().position(|other| other == hash))
            .unwrap_or(0) as isize;
        let count = self.order.len() as isize;
        let next = (position + delta).rem_euclid(count) as usize;
        self.focus(self.order[next].clone());
    }

    /// Drops the focused session from the list. The torrent stays in your
    /// TorBox account — this closes the tab, it does not delete anything.
    fn close_current(&mut self) {
        let Some(hash) = self.current.clone() else { return };
        let Some(position) = self.order.iter().position(|other| *other == hash) else { return };
        self.order.remove(position);
        self.sessions.remove(&hash);
        self.pending.remove(&hash);
        self.queued_play.remove(&hash);

        // Fall back to the tab on the left, the way a browser does.
        match self.order.get(position.saturating_sub(1)).cloned() {
            Some(next) => self.focus(next),
            None => {
                self.current = None;
                self.mode = Mode::Entry;
                self.input.clear();
                self.status = "no sessions left — paste a magnet link".into();
            }
        }
        self.persist();
    }

    /// Shows `hash` and reports what state it is in — a session reached by
    /// switching may still be fetching, or may have finished while hidden.
    fn focus(&mut self, hash: String) {
        let fetching = self.pending.get(&hash).map(|p| p.progress);
        let Some(session) = self.sessions.get_mut(&hash) else { return };
        session.clamp_selection();
        self.status = session.summary(fetching);
        self.current = Some(hash);
        self.mode = Mode::Browse;
    }

    pub fn handle(&mut self, input: Input) {
        match input {
            Input::Key(key) => self.on_key(key),
            Input::Job(job) => self.on_job(job),
            Input::Tick => self.reap(),
        }
    }

    /// Drops mpv processes that have exited, so the quit guard stays accurate.
    fn reap(&mut self) {
        self.playing.retain_mut(|p| !p.finished());
        if self.playing.is_empty() {
            self.quit_armed = false;
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        self.error = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        match self.mode {
            Mode::Entry => self.on_key_entry(key),
            Mode::Browse => self.on_key_browse(key),
            Mode::ConfirmFetch => self.on_key_confirm(key),
        }
    }

    fn on_key_entry(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let uri = self.input.trim().to_string();
                if !uri.is_empty() {
                    self.submit(uri);
                }
            }
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match read_clipboard() {
                    Some(text) => self.input = text,
                    None => self.status = "clipboard holds no magnet link".into(),
                }
            }
            // Entry is an overlay over the sessions, so switching from it is
            // what you want — typing a magnet is not a reason to be stuck here.
            KeyCode::Tab => self.switch(1),
            KeyCode::BackTab => self.switch(-1),
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => {
                if self.session().is_some() {
                    self.mode = Mode::Browse;
                } else {
                    self.request_quit();
                }
            }
            _ => {}
        }
    }

    fn on_key_browse(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('n') => {
                self.mode = Mode::Entry;
                self.input.clear();
                self.status = "paste a magnet link and press Enter".into();
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Tab => self.switch(1),
            KeyCode::BackTab => self.switch(-1),
            KeyCode::Char('x') => self.close_current(),
            KeyCode::Enter => self.play_selected(),
            _ => {}
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let Some(hash) = self.current.clone() else { return };
                let (uri, name) = match self.sessions.get(&hash) {
                    Some(s) => (s.uri.clone(), s.name.clone()),
                    None => return,
                };
                self.pending.insert(
                    hash.clone(),
                    Pending { name: name.clone(), progress: 0.0, state: "queued".into() },
                );
                self.mode = Mode::Entry;
                self.input.clear();
                self.status = format!("fetching {name} in the background — you can paste another magnet");
                self.spawn_fetch(hash, uri);
            }
            _ => {
                self.mode = Mode::Entry;
                self.status = "cancelled — no quota spent".into();
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(session) = self.session_mut() else { return };
        let count = session.files.len();
        if count == 0 {
            return;
        }
        let current = session.list.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, count as isize - 1);
        session.list.select(Some(next as usize));
    }

    fn request_quit(&mut self) {
        self.reap();
        if self.playing.is_empty() {
            self.should_quit = true;
            return;
        }
        if self.quit_armed {
            self.should_quit = true;
        } else {
            self.quit_armed = true;
            let count = self.playing.len();
            let plural = if count == 1 { "stream" } else { "streams" };
            self.status = format!("{count} {plural} playing — press q again to kill and quit");
        }
    }

    /// Parses the magnet and starts the `checkcached` lookup.
    fn submit(&mut self, uri: String) {
        let parsed = match magnet::parse(&uri) {
            Ok(m) => m,
            Err(err) => {
                self.error = Some(err.to_string());
                return;
            }
        };
        let name = parsed.display_name.clone().unwrap_or_else(|| parsed.hash.clone());
        self.status = format!("checking {name}…");

        let torbox = self.torbox.clone();
        let tx = self.tx.clone();
        let hash = parsed.hash.clone();
        tokio::spawn(async move {
            match torbox.check_cached(&hash).await {
                Ok(Some(files)) => {
                    let _ = tx.send(Job::Checked {
                        hash: hash.clone(),
                        uri: uri.clone(),
                        name,
                        files,
                        cached: true,
                    });
                    // Cached torrents are instant to add, and adding is what
                    // yields the file ids `requestdl` needs. Do it now so the
                    // ids are waiting by the time a file is picked.
                    prepare(torbox, tx, hash, uri).await;
                }
                Ok(None) => {
                    let _ = tx.send(Job::Checked {
                        hash,
                        uri,
                        name,
                        files: Vec::new(),
                        cached: false,
                    });
                }
                Err(err) => {
                    let _ = tx.send(Job::Failed { message: err.to_string() });
                }
            }
        });
    }

    /// Uncached path: add the torrent, then poll until TorBox has the data.
    fn spawn_fetch(&mut self, hash: String, uri: String) {
        let torbox = self.torbox.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let torrent_id = match torbox.create_torrent(&uri).await {
                Ok(id) => id,
                Err(err) => {
                    let _ = tx.send(Job::Failed { message: err.to_string() });
                    return;
                }
            };

            let started = Instant::now();
            loop {
                let interval = if started.elapsed() < FAST_POLL_WINDOW { FAST_POLL } else { SLOW_POLL };
                tokio::time::sleep(interval).await;

                match torbox.torrent_status(torrent_id).await {
                    Ok(status) if status.download_present => {
                        let _ = tx.send(Job::Ready { hash, torrent_id, files: status.files });
                        return;
                    }
                    Ok(status) => {
                        let _ = tx.send(Job::Progress {
                            hash: hash.clone(),
                            progress: status.progress,
                            state: status.state,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(Job::Failed { message: err.to_string() });
                        return;
                    }
                }
            }
        });
    }

    fn on_job(&mut self, job: Job) {
        match job {
            Job::Checked { hash, uri, name, files, cached } => {
                let (display, filtered) = files::filter_videos(&files);
                let ready = match self.sessions.get_mut(&hash) {
                    // Pasting a magnet we already hold refreshes it in place.
                    // A ready session keeps its listing: `checkcached` names
                    // carry no file ids, and overwriting would lose them.
                    Some(session) => {
                        session.name = name.clone();
                        session.uri = uri;
                        if !session.ready {
                            session.files = display;
                            session.filtered = filtered;
                            session.clamp_selection();
                        }
                        session.ready
                    }
                    None => {
                        self.order.push(hash.clone());
                        self.sessions
                            .insert(hash.clone(), Session::new(name.clone(), uri, display, filtered));
                        false
                    }
                };

                // An already-downloaded torrent is playable whatever
                // `checkcached` says, so never re-prompt for one.
                if cached || ready {
                    self.focus(hash);
                } else {
                    self.current = Some(hash);
                    self.mode = Mode::ConfirmFetch;
                    self.status = format!("{name} is not cached");
                }
                self.persist();
            }
            Job::Ready { hash, torrent_id, files } => {
                self.pending.remove(&hash);
                let (display, filtered) = files::filter_videos(&files);
                let name = if let Some(session) = self.sessions.get_mut(&hash) {
                    session.torrent_id = Some(torrent_id);
                    session.files = display;
                    session.filtered = filtered;
                    session.ready = true;
                    session.clamp_selection();
                    session.name.clone()
                } else {
                    return;
                };

                if let Some(wanted) = self.queued_play.remove(&hash) {
                    self.launch(&hash, &wanted);
                } else if self.current.as_deref() == Some(hash.as_str()) && self.mode != Mode::Browse {
                    self.mode = Mode::Browse;
                    self.status = format!("{name} — ready");
                } else if self.current.as_deref() != Some(hash.as_str()) {
                    // Another session finished; say so without stealing focus.
                    self.status = format!("{name} is ready — tab to it");
                }
                // The ids just learned are what make a restored tab playable.
                self.persist();
            }
            Job::Progress { hash, progress, state } => {
                if let Some(pending) = self.pending.get_mut(&hash) {
                    pending.progress = progress;
                    pending.state = state;
                }
            }
            Job::Failed { message } => self.error = Some(message),
        }
    }

    fn play_selected(&mut self) {
        let Some(hash) = self.current.clone() else { return };
        let Some(session) = self.sessions.get(&hash) else { return };
        let index = session.list.selected().unwrap_or(0);
        let Some(file) = session.files.get(index) else {
            self.error = Some("nothing to play".into());
            return;
        };
        let name = file.name.clone();

        if session.ready {
            self.launch(&hash, &name);
        } else {
            // `checkcached` gave names but no ids; the add is already running.
            self.queued_play.insert(hash, name.clone());
            self.status = format!("preparing {}…", short(&name));
        }
    }

    fn launch(&mut self, hash: &str, file_name: &str) {
        let Some(session) = self.sessions.get(hash) else { return };
        let Some(torrent_id) = session.torrent_id else {
            self.error = Some("torrent has no id yet".into());
            return;
        };
        // Ids arrive from `mylist`, whose names may be prefixed differently
        // from `checkcached`, so fall back to matching the basename.
        let file = session
            .files
            .iter()
            .find(|f| f.name == file_name && f.id.is_some())
            .or_else(|| {
                session
                    .files
                    .iter()
                    .find(|f| f.short_name() == short(file_name) && f.id.is_some())
            });
        let Some(file) = file else {
            self.error = Some(format!("TorBox has no file id for {}", short(file_name)));
            return;
        };
        let file_id = file.id.expect("filtered on is_some");

        let url = self.proxy.publish(torrent_id, file_id);
        let token = format!("{torrent_id}-{file_id}");
        let title = short(&file.name).to_string();
        match player::spawn(&url, &title, &player::socket_path(&token)) {
            Ok(playing) => {
                self.status = format!("playing {title}");
                self.playing.push(playing);
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    pub async fn shutdown(&mut self) {
        self.persist();
        for playing in &mut self.playing {
            playing.kill().await;
        }
    }
}

fn short(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Adds a cached torrent and reports its files, which carry the ids needed by
/// `requestdl`.
async fn prepare(torbox: Torbox, tx: UnboundedSender<Job>, hash: String, uri: String) {
    let torrent_id = match torbox.create_torrent(&uri).await {
        Ok(id) => id,
        Err(err) => {
            let _ = tx.send(Job::Failed { message: err.to_string() });
            return;
        }
    };
    match torbox.torrent_status(torrent_id).await {
        Ok(status) => {
            let _ = tx.send(Job::Ready { hash, torrent_id, files: status.files });
        }
        Err(err) => {
            let _ = tx.send(Job::Failed { message: err.to_string() });
        }
    }
}

/// macOS only, so `pbpaste` is the whole clipboard story.
pub fn read_clipboard() -> Option<String> {
    let output = std::process::Command::new("pbpaste").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if magnet::looks_like_magnet(&text) {
        Some(text)
    } else {
        None
    }
}

pub fn spawn_key_reader(tx: UnboundedSender<Input>) -> Result<()> {
    std::thread::spawn(move || {
        use ratatui::crossterm::event::{self, Event};
        loop {
            match event::read() {
                Ok(Event::Key(key)) if key.is_press() => {
                    if tx.send(Input::Key(key)).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const GB: u64 = 1024 * 1024 * 1024;

    pub(super) async fn test_app() -> App {
        let torbox = Torbox::new("test-key".into()).unwrap();
        let proxy = crate::proxy::start(torbox.clone()).await.unwrap();
        // The receiver is dropped: these tests drive `on_job` directly and
        // never take the paths that spawn background work.
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(torbox, proxy, tx);
        // Never touch the real session file from a test.
        app.store_path = None;
        app
    }

    /// An app whose session list lives in a throwaway file.
    pub(super) async fn stored_app(path: &std::path::Path) -> App {
        let mut app = test_app().await;
        app.store_path = Some(path.to_path_buf());
        app
    }

    pub(super) fn temp_store(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("streamtui-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("sessions.json")
    }

    pub(super) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(super) fn files(names: &[&str]) -> Vec<TorrentFile> {
        names
            .iter()
            .map(|name| TorrentFile { id: None, name: (*name).to_string(), size: GB })
            .collect()
    }

    pub(super) fn checked(app: &mut App, hash: &str, name: &str, names: &[&str], cached: bool) {
        app.on_job(Job::Checked {
            hash: hash.to_string(),
            uri: format!("magnet:?xt=urn:btih:{hash}"),
            name: name.to_string(),
            files: files(names),
            cached,
        });
    }

    #[tokio::test]
    async fn tab_cycles_sessions_and_keeps_each_cursor() {
        let mut app = test_app().await;
        checked(&mut app, "aaa", "First", &["a1.mkv", "a2.mkv"], true);
        app.on_key(key(KeyCode::Down));
        checked(&mut app, "bbb", "Second", &["b1.mkv"], true);

        assert_eq!(app.current.as_deref(), Some("bbb"));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.current.as_deref(), Some("aaa"));
        assert_eq!(app.session().unwrap().list.selected(), Some(1));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.current.as_deref(), Some("bbb"));
        assert_eq!(app.session().unwrap().list.selected(), Some(0));
    }

    #[tokio::test]
    async fn shift_tab_walks_the_ring_backwards() {
        let mut app = test_app().await;
        for hash in ["aaa", "bbb", "ccc"] {
            checked(&mut app, hash, hash, &["file.mkv"], true);
        }
        assert_eq!(app.current.as_deref(), Some("ccc"));
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.current.as_deref(), Some("bbb"));
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.current.as_deref(), Some("aaa"));
        // Wraps past the start.
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.current.as_deref(), Some("ccc"));
    }

    #[tokio::test]
    async fn a_single_session_says_why_tab_does_nothing() {
        let mut app = test_app().await;
        checked(&mut app, "aaa", "First", &["a1.mkv"], true);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.current.as_deref(), Some("aaa"));
        assert_eq!(app.order.len(), 1);
        assert!(app.status.contains("only one session"), "status was {:?}", app.status);
    }

    #[tokio::test]
    async fn tab_works_from_the_magnet_entry_field_too() {
        let mut app = test_app().await;
        checked(&mut app, "aaa", "First", &["a1.mkv"], true);
        checked(&mut app, "bbb", "Second", &["b1.mkv"], true);

        // `n` opens the entry field over the sessions.
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.mode, Mode::Entry);

        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.current.as_deref(), Some("aaa"));
    }

    #[tokio::test]
    async fn tab_is_inert_before_any_session_exists() {
        let mut app = test_app().await;
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.mode, Mode::Entry);
        assert!(app.current.is_none());
    }

    #[tokio::test]
    async fn playing_a_file_uses_the_focused_session_cursor() {
        let mut app = test_app().await;
        checked(&mut app, "aaa", "First", &["a1.mkv", "a2.mkv"], true);
        app.on_key(key(KeyCode::Down));
        checked(&mut app, "bbb", "Second", &["b1.mkv"], true);
        app.on_key(key(KeyCode::Tab));
        // Not ready, so the pick is queued rather than launched — which is
        // what tells us which file the cursor resolved to.
        app.play_selected();
        assert_eq!(app.queued_play.get("aaa").map(String::as_str), Some("a2.mkv"));
    }

    #[tokio::test]
    async fn resubmitting_a_known_magnet_keeps_its_ids_and_tab() {
        let mut app = test_app().await;
        checked(&mut app, "aaa", "First", &["a1.mkv"], true);
        app.on_job(Job::Ready {
            hash: "aaa".into(),
            torrent_id: 7,
            files: vec![TorrentFile { id: Some(3), name: "a1.mkv".into(), size: GB }],
        });

        // `checkcached` reports names without ids; re-pasting must not lose them.
        checked(&mut app, "aaa", "First", &["a1.mkv"], true);

        let session = app.session().unwrap();
        assert_eq!(session.torrent_id, Some(7));
        assert!(session.ready);
        assert_eq!(session.files[0].id, Some(3));
        assert_eq!(app.order, vec!["aaa".to_string()]);
    }

    #[tokio::test]
    async fn a_downloaded_torrent_is_never_offered_for_fetching_again() {
        let mut app = test_app().await;
        checked(&mut app, "aaa", "First", &[], false);
        assert_eq!(app.mode, Mode::ConfirmFetch);

        app.on_job(Job::Ready {
            hash: "aaa".into(),
            torrent_id: 7,
            files: vec![TorrentFile { id: Some(3), name: "a1.mkv".into(), size: GB }],
        });
        checked(&mut app, "aaa", "First", &[], false);
        assert_eq!(app.mode, Mode::Browse);
    }

    #[tokio::test]
    async fn switching_to_a_fetching_session_reports_its_progress() {
        let mut app = test_app().await;
        checked(&mut app, "aaa", "First", &[], false);
        app.pending.insert(
            "aaa".into(),
            Pending { name: "First".into(), progress: 0.42, state: "downloading".into() },
        );
        checked(&mut app, "bbb", "Second", &["b1.mkv"], true);

        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.current.as_deref(), Some("aaa"));
        assert!(app.status.contains("42%"), "status was {:?}", app.status);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::tests::*;
    use super::*;

    #[tokio::test]
    async fn sessions_come_back_after_a_restart() {
        let path = temp_store("restart");
        {
            let mut app = stored_app(&path).await;
            checked(&mut app, "aaa", "First", &["a1.mkv", "a2.mkv"], true);
            app.on_job(Job::Ready {
                hash: "aaa".into(),
                torrent_id: 7,
                files: vec![TorrentFile { id: Some(3), name: "a1.mkv".into(), size: GB }],
            });
            checked(&mut app, "bbb", "Second", &["b1.mkv"], true);
            app.shutdown().await;
        }

        let mut next = stored_app(&path).await;
        next.restore();

        assert_eq!(next.order, vec!["aaa".to_string(), "bbb".to_string()]);
        assert_eq!(next.mode, Mode::Browse);
        // The tab that had focus is the one you come back to.
        assert_eq!(next.current.as_deref(), Some("bbb"));

        let restored = next.sessions.get("aaa").unwrap();
        assert_eq!(restored.torrent_id, Some(7));
        assert!(restored.ready);
        // Ids survive, so a restored tab plays without another API call.
        assert_eq!(restored.files[0].id, Some(3));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn the_cursor_position_survives_a_restart() {
        let path = temp_store("cursor");
        {
            let mut app = stored_app(&path).await;
            checked(&mut app, "aaa", "First", &["a1.mkv", "a2.mkv", "a3.mkv"], true);
            app.on_key(key(KeyCode::Down));
            app.on_key(key(KeyCode::Down));
            app.shutdown().await;
        }

        let mut next = stored_app(&path).await;
        next.restore();
        assert_eq!(next.session().unwrap().list.selected(), Some(2));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn closing_a_session_drops_it_from_the_saved_list() {
        let path = temp_store("close");
        {
            let mut app = stored_app(&path).await;
            checked(&mut app, "aaa", "First", &["a1.mkv"], true);
            checked(&mut app, "bbb", "Second", &["b1.mkv"], true);
            app.on_key(key(KeyCode::Char('x')));
            assert_eq!(app.order, vec!["aaa".to_string()]);
            // Focus falls back to the neighbour on the left.
            assert_eq!(app.current.as_deref(), Some("aaa"));
        }

        let mut next = stored_app(&path).await;
        next.restore();
        assert_eq!(next.order, vec!["aaa".to_string()]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn closing_the_last_session_returns_to_the_magnet_field() {
        let path = temp_store("close-last");
        let mut app = stored_app(&path).await;
        checked(&mut app, "aaa", "First", &["a1.mkv"], true);
        app.on_key(key(KeyCode::Char('x')));

        assert!(app.order.is_empty());
        assert!(app.current.is_none());
        assert_eq!(app.mode, Mode::Entry);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn a_corrupt_or_missing_file_starts_empty_rather_than_failing() {
        let path = temp_store("corrupt");
        let mut app = stored_app(&path).await;
        // Missing.
        app.restore();
        assert!(app.order.is_empty());

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json at all").unwrap();
        app.restore();
        assert!(app.order.is_empty());

        // A file from a future version is discarded, not misread.
        std::fs::write(&path, r#"{"version":99,"sessions":[{"hash":"z","uri":"u","name":"n"}]}"#).unwrap();
        app.restore();
        assert!(app.order.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn the_saved_list_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_store("perms");
        let mut app = stored_app(&path).await;
        checked(&mut app, "aaa", "First", &["a1.mkv"], true);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "magnets are a record of what you watch");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
