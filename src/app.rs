//! TUI state machine and the background jobs that feed it.

use crate::files::{self, TorrentFile};
use crate::magnet;
use crate::player::{self, Playing};
use crate::proxy::ProxyHandle;
use crate::torbox::Torbox;
use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use std::collections::HashMap;
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

#[derive(PartialEq, Eq, Clone, Copy)]
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
    pub list: ListState,
    pub sessions: HashMap<String, Session>,
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
}

impl App {
    pub fn new(torbox: Torbox, proxy: ProxyHandle, tx: UnboundedSender<Job>) -> Self {
        Self {
            mode: Mode::Entry,
            input: String::new(),
            status: "paste a magnet link and press Enter".into(),
            error: None,
            list: ListState::default(),
            sessions: HashMap::new(),
            current: None,
            pending: HashMap::new(),
            playing: Vec::new(),
            queued_play: HashMap::new(),
            quit_armed: false,
            should_quit: false,
            torbox,
            proxy,
            tx,
        }
    }

    pub fn session(&self) -> Option<&Session> {
        self.current.as_ref().and_then(|hash| self.sessions.get(hash))
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
        let Some(count) = self.session().map(|s| s.files.len()) else { return };
        if count == 0 {
            return;
        }
        let current = self.list.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, count as isize - 1);
        self.list.select(Some(next as usize));
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
                self.sessions.insert(
                    hash.clone(),
                    Session { name: name.clone(), uri, torrent_id: None, files: display, filtered, ready: false },
                );
                self.current = Some(hash);
                if cached {
                    self.mode = Mode::Browse;
                    self.list.select(Some(0));
                    let session = self.session().expect("just inserted");
                    self.status = if session.filtered {
                        format!("{} — {} video file(s)", name, session.files.len())
                    } else {
                        format!("{name} — no video files matched, showing everything")
                    };
                } else {
                    self.mode = Mode::ConfirmFetch;
                    self.status = format!("{name} is not cached");
                }
            }
            Job::Ready { hash, torrent_id, files } => {
                self.pending.remove(&hash);
                let (display, filtered) = files::filter_videos(&files);
                let name = if let Some(session) = self.sessions.get_mut(&hash) {
                    session.torrent_id = Some(torrent_id);
                    session.files = display;
                    session.filtered = filtered;
                    session.ready = true;
                    session.name.clone()
                } else {
                    return;
                };

                if let Some(wanted) = self.queued_play.remove(&hash) {
                    self.launch(&hash, &wanted);
                } else if self.current.as_deref() == Some(hash.as_str()) && self.mode != Mode::Browse {
                    self.mode = Mode::Browse;
                    self.list.select(Some(0));
                    self.status = format!("{name} — ready");
                } else if self.current.as_deref() != Some(hash.as_str()) {
                    self.status = format!("{name} is ready");
                }
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
        let index = self.list.selected().unwrap_or(0);
        let Some(session) = self.sessions.get(&hash) else { return };
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
