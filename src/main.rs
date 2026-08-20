//! streamtui — paste a magnet, pick a file, watch it in mpv.
//!
//! TorBox is the only backend: there is no BitTorrent protocol code here. The
//! flow is checkcached → createtorrent → requestdl, with a localhost proxy in
//! front of the CDN so the API key never reaches mpv.

mod app;
mod config;
mod files;
mod magnet;
mod player;
mod proxy;
mod torbox;
mod ui;

use anyhow::{Context, Result};
use app::{App, Input};
use clap::Parser;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "streamtui", about = "Stream torrents from TorBox into mpv")]
struct Args {
    /// Magnet link to open. Falls back to the clipboard, then to an input field.
    magnet: Option<String>,
    /// Store a TorBox API key and exit.
    #[arg(long)]
    set_key: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.set_key {
        let path = config::save_key(&prompt_key()?)?;
        println!("saved to {}", path.display());
        return Ok(());
    }

    let key = match config::load_key()? {
        Some(key) => key,
        None => {
            println!("streamtui needs a TorBox API key (from https://torbox.app settings).");
            let key = prompt_key()?;
            let path = config::save_key(&key)?;
            println!("saved to {}", path.display());
            key
        }
    };

    let torbox = torbox::Torbox::new(key)?;
    let proxy = proxy::start(torbox.clone()).await?;

    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel();
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    app::spawn_key_reader(input_tx.clone())?;

    // Jobs and keystrokes share one queue so the draw loop has a single source.
    {
        let input_tx = input_tx.clone();
        tokio::spawn(async move {
            while let Some(job) = job_rx.recv().await {
                if input_tx.send(Input::Job(job)).is_err() {
                    return;
                }
            }
        });
    }
    {
        let input_tx = input_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(250));
            loop {
                ticker.tick().await;
                if input_tx.send(Input::Tick).is_err() {
                    return;
                }
            }
        });
    }

    let mut app = App::new(torbox, proxy, job_tx);
    if let Some(magnet) = args.magnet.or_else(app::read_clipboard) {
        app.input = magnet;
        app.handle(Input::Key(key_event(ratatui::crossterm::event::KeyCode::Enter)));
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &mut input_rx).await;
    ratatui::restore();
    app.shutdown().await;
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Input>,
) -> Result<()> {
    terminal.draw(|frame| ui::draw(frame, app))?;
    while let Some(input) = input_rx.recv().await {
        app.handle(input);
        if app.should_quit {
            return Ok(());
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
    }
    Ok(())
}

fn key_event(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
}

fn prompt_key() -> Result<String> {
    use std::io::Write;
    print!("TorBox API key: ");
    std::io::stdout().flush()?;
    let mut key = String::new();
    std::io::stdin().read_line(&mut key).context("reading the API key")?;
    Ok(key.trim().to_string())
}
