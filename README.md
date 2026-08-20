# streamtui

Paste a magnet link, pick a file, watch it in mpv. macOS.

TorBox is the only backend — there is no BitTorrent protocol code here.
The flow is `checkcached` → `createtorrent` → `requestdl`, with a localhost
proxy in front of the CDN so the API key never reaches mpv.

## Requirements

- mpv on `PATH`
- A TorBox API key

## Install

```sh
cargo build --release
cp target/release/streamtui /usr/local/bin/
```

## Use

```sh
streamtui                       # reads the clipboard, else opens an input field
streamtui 'magnet:?xt=urn:btih:…'
streamtui --set-key             # store the API key and exit
```

The key comes from `TORBOX_API_KEY` if set, otherwise
`$XDG_CONFIG_HOME/streamtui/config.toml` (written `0600`).

## Keys

| Key | Action |
|---|---|
| `enter` | submit magnet / play selected file |
| `↑ ↓` `j k` | move selection |
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

**Playback**: mpv is handed `http://127.0.0.1:<port>/stream/<token>`. The proxy
forwards `Range` and `If-Range` verbatim, so mpv seeks natively against the CDN
and does its own readahead — there is no buffering layer of our own. mpv runs
with `--no-terminal` and its stdio on `/dev/null`, because ratatui owns the tty.
An `--input-ipc-server` socket is opened but unused, ready for playback state.

Torrents stay in your TorBox account after playback, keeping rewatches instant.

## Not in v1

Watch history, resume position, library management, P2P fallback.
