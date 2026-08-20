//! mpv process handling.
//!
//! mpv must not touch the terminal: ratatui holds it in raw mode on the
//! alternate screen, and mpv's own terminal handling would corrupt the render.

use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::{Child, Command};

pub struct Playing {
    child: Child,
}

impl Playing {
    /// True once mpv has exited. Never blocks.
    pub fn finished(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)) | Err(_))
    }

    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

pub fn spawn(url: &str, title: &str, ipc_socket: &str) -> Result<Playing> {
    let child = Command::new("mpv")
        // Leave the terminal entirely to the TUI.
        .arg("--no-terminal")
        // Show a window at once instead of after the first decoded frame.
        .arg("--force-window=immediate")
        .arg(format!("--title={title}"))
        // Unused in v1, but the socket exists the moment we want playback state.
        .arg(format!("--input-ipc-server={ipc_socket}"))
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start mpv — is it installed and on PATH?")?;

    Ok(Playing { child })
}

pub fn socket_path(token: &str) -> String {
    format!("/tmp/streamtui-{token}.sock")
}
