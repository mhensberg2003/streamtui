# streamtui

Paste a magnet link, pick a file, watch it in mpv. macOS and Linux.

TorBox is the only backend — there is no BitTorrent protocol code here.
The flow is `checkcached` → `createtorrent` → `requestdl`, with a localhost
proxy in front of the CDN so the API key never reaches mpv.

## Requirements

- macOS or Linux
- [Rust](https://rustup.rs) 1.85 or later (the crate uses edition 2024)
- [mpv](https://mpv.io) on `PATH`
- A [TorBox](https://torbox.app) API key (Settings → API Key)
- Linux only, for clipboard paste: `wl-clipboard` (Wayland), or `xclip` or
  `xsel` (X11). Without one, paste the magnet with your terminal's own paste
  key, or pass it as an argument.

## Install

1. Install Rust, mpv and (on Linux) a C compiler and a clipboard tool.

   macOS:

   ```sh
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   brew install mpv
   ```

   Arch Linux:

   ```sh
   sudo pacman -S --needed rustup base-devel mpv wl-clipboard   # or xclip on X11
   rustup default stable
   ```

   Other distributions: install the same packages with your package manager.
   `base-devel` (or `build-essential`) is needed because the TLS library
   compiles C code.

2. Install streamtui. Either let cargo build it into `~/.cargo/bin`:

   ```sh
   cargo install --git https://github.com/mhensberg2003/streamtui
   ```

   Make sure `~/.cargo/bin` is on your `PATH` (rustup adds it on macOS; on
   Arch, add `export PATH="$HOME/.cargo/bin:$PATH"` to your shell profile).

   Or clone it and install the binary system-wide:

   ```sh
   git clone https://github.com/mhensberg2003/streamtui
   cd streamtui
   cargo build --release
   sudo install -S -m 755 target/release/streamtui /usr/local/bin/streamtui   # macOS
   sudo install -m 755 target/release/streamtui /usr/local/bin/streamtui      # Linux
   ```

   On macOS, use `install -S` for updates too: it replaces the executable
   atomically, avoiding code-signing cache failures from overwriting it in
   place. Leave it out on Linux: there `-S` sets a backup suffix, and GNU
   `install` removes the old file before it copies, so updates are safe
   without it.

3. Store your TorBox API key:

   ```sh
   streamtui --set-key
   ```

To update, run the same install command again (add `--force` to
`cargo install`, or `git pull` first when building from a clone).

## Use

```sh
streamtui                       # reads the clipboard, else opens an input field
streamtui 'magnet:?xt=urn:btih:…'
streamtui --set-key             # store the API key and exit
```

The key comes from `TORBOX_API_KEY` if set, otherwise
`streamtui/config.toml` (written `0600`) under `$XDG_CONFIG_HOME`. When that is
not set, the default is `~/.config` on Linux and `~/Library/Application Support`
on macOS.

## Keys

| Key | Action |
|---|---|
| `enter` | submit magnet / play selected file |
| `↑ ↓` `j k` | move selection |
| `tab` `shift+tab` | switch session (needs two or more) |
| `x` | close the session |
| `n` | new magnet |
| `ctrl+v` | paste from the clipboard |
| `y` `n` | answer the "not cached, fetch it?" prompt |
| `q` | quit — twice if streams are playing |

## How it works

**Cached torrents** (the fast path): `checkcached?list_files=true` returns the
file list in under a second. The torrent is added in the background at the same
time, because adding is what produces the file ids `requestdl` needs — so by
the time a file is picked, the ids are usually already there.

**Uncached torrents**: adding one spends a download slot and one of just
**60 uncached fetches per hour**, so it asks first. The fetch then runs as a
background job — the TUI stays live and you can paste another magnet meanwhile.
Progress polls `mylist?bypass_cache=true` every 2s for the first 30s, then every
10s. Without `bypass_cache` the server's answer is up to 600 seconds stale.

**Sessions**: every magnet you paste opens a session, and they all stay open.
`tab` walks the ring of them — from the file list or from the magnet entry
field — so you can queue an uncached fetch, watch something cached while it
runs, and come back. Each session keeps its own cursor. A tab strip appears
under the title once there is more than one, with
`⟳` on the ones still fetching. Re-pasting a magnet you already hold switches
to its session instead of starting over. `x` closes a tab — the torrent stays
in your TorBox account, only the tab goes.

Sessions outlive the process. They are written to `sessions.json` beside
`config.toml` (also `0600` — a list of magnets says what you watch), and come
back on the next launch with their file lists, torrent ids and cursor. Restored
tabs are playable straight away, with no API round trip. The 20 most recent are
kept; a missing, corrupt or older file is discarded silently rather than
failing startup.

**Playback**: mpv is handed `http://127.0.0.1:<port>/stream/<token>`. The proxy
forwards `Range` and `If-Range` verbatim, so mpv seeks natively against the CDN
and does its own readahead — there is no buffering layer of our own. mpv runs
with `--no-terminal` and its stdio on `/dev/null`, because ratatui owns the tty.
An `--input-ipc-server` socket is opened but unused, ready for playback state.

The proxy caches each file's CDN URL in memory for two hours, so seeks and
track switches reuse it without another TorBox API lookup. Concurrent requests
share one lookup per file. A CDN 401, 403, or 410 triggers one URL refresh and
retry; downloaded media bytes are not cached.

Torrents stay in your TorBox account after playback, keeping rewatches instant.

## Not in v1

Watch history, resume position, library management, P2P fallback. A restored
session trusts its saved torrent id: if you removed the torrent from TorBox
between runs, playback fails rather than re-adding it.
