# mikan

**English** | [简体中文](README.md)

An interactive command-line downloader for [Mikan Project](https://mikanani.me) anime RSS feeds.

Point it at a feed URL and it walks you through picking episodes and sending them
wherever you want — saved as `.torrent` files, written out as a plain URL list, or
handed straight to qBittorrent over its WebUI API. There's also a fully
non-interactive mode for scripts and cron.

```
$ mikan "https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"

? Mikan Project - 石纪元 科学与未来 第3部分
> [x] [猎户压制部] 新石纪 第四季 [37] [1080p] [繁日内嵌]  (835.0 MiB, 2026-06-28)
  [ ] [猎户压制部] 新石纪 第四季 [36] [1080p] [繁日内嵌]  (842.1 MiB, 2026-06-21)
  ...
  type to filter · space: toggle · →/←: all/none · enter: next · esc: cancel
```

## Features

| | |
|---|---|
| **Interactive wizard** | Multi-select episode picker showing size and publish date, then choose one or more export targets. |
| **Three export targets** | Download `.torrent` files, write a newline-delimited URL list, or add directly to qBittorrent. |
| **qBittorrent integration** | Save named connection profiles, auto-create a category per feed, upload torrents via the WebUI API. |
| **Non-interactive mode** | Drive the whole flow from flags (`--all` / `--latest` / `--filter`) — no prompts, suitable for cron. |
| **Proxy aware** | `--proxy`, standard proxy env vars, or the macOS system proxy — `mikanani.me` is frequently DNS-poisoned. |
| **Safe by construction** | Strips control / bidi / zero-width characters from remote feed text, sanitizes filenames, and stores the config file with owner-only permissions. |

## Install

Requires a recent Rust toolchain (edition 2024, Rust 1.85+).

### From source

```sh
git clone <this-repo> mikanani-cli
cd mikanani-cli
cargo install --path .
```

This installs a binary named `mikan` into `~/.cargo/bin`.

### With Nix

A flake is provided:

```sh
nix run .                # run without installing
nix build .             # build into ./result
nix develop             # drop into a dev shell with the toolchain
```

## Usage

### Interactive wizard

Pass a Mikan RSS feed URL and follow the prompts:

```sh
mikan "https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"
```

The wizard has up to four steps:

1. **Pick episodes** — a multi-select list, sorted newest-first when every episode is dated (otherwise the feed's own order is kept). Space toggles, `→`/`←` select all/none, Enter continues, Esc cancels.
2. **Choose export formats** — any combination of *download `.torrent` files*, *write a URL list*, and *add to qBittorrent*.
3. **Export path** — asked only when something is written to disk. Remembers your last-used paths and offers them as history-backed autocomplete (`↑`/`↓` to browse, Tab to fill). `~` is expanded.
4. **qBittorrent profile + category** — asked only when adding to qBittorrent. Picks a saved profile (or creates one inline), then asks for a category (defaulting to the sanitized feed title).

### qBittorrent profiles

Connection details for qBittorrent's WebUI are stored as named profiles so you don't
re-enter them each run. An empty username uses qBittorrent's "bypass authentication
for localhost" mode.

```sh
mikan qbt set [name]        # create/update a profile interactively (default name: "default")
mikan qbt list              # list saved profiles
mikan qbt test [name]       # connect and print the qBittorrent version
mikan qbt remove <name>     # delete a profile
```

`qbt set` and `qbt test` connect before saving, so a typo in the endpoint or
credentials is caught immediately.

### Non-interactive mode

Add `-y`/`--yes` to run without any prompts. You must specify **what to select** and
**where to send it**:

```sh
# Newest 3 episodes → download .torrent files into ./torrents
mikan -y --latest 3 --out ./torrents "https://mikanani.me/RSS/Bangumi?bangumiId=3950&subgroupid=597"

# Everything → add to the "default" qBittorrent profile
mikan -y --all --qbt "https://mikanani.me/RSS/..."

# Only 1080p episodes → a named profile, custom category
mikan -y --filter 1080p --qbt=seedbox --category "Dr. Stone" "https://mikanani.me/RSS/..."

# Combine targets: write a URL list AND download the files
mikan -y --all --url-list ./lists --out ./torrents "https://mikanani.me/RSS/..."
```

**Selection flags** (choose at least one):

| Flag | Effect |
|---|---|
| `--all` | Every episode in the feed. |
| `--latest N` | The newest `N` episodes. |
| `--filter TEXT` | Episodes whose title contains `TEXT` (case-insensitive). Composes with `--latest`. |

**Output flags** (choose at least one):

| Flag | Effect |
|---|---|
| `--out DIR` | Download `.torrent` files into `DIR`. |
| `--url-list DIR` | Write the torrent-URL list into `DIR`. |
| `--qbt[=PROFILE]` | Add to qBittorrent using a saved profile (bare `--qbt` uses `default`). |
| `--category NAME` | qBittorrent category (defaults to the sanitized feed title). |

In `-y` mode the qBittorrent profile is read-only — profiles are never created or
saved — so it's safe to run unattended. Set up the profile once interactively with
`mikan qbt set` first.

### Proxy

`mikanani.me` is often unreachable behind DNS poisoning. If a fetch fails with a
connection/TLS/gateway error, the tool suggests a proxy. Provide one via:

```sh
mikan --proxy http://127.0.0.1:7890 "https://mikanani.me/RSS/..."
```

Otherwise a proxy is detected automatically:

- The standard `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` environment variables are honored on **every** platform.
- The OS-level system proxy is picked up on **macOS** (Network settings) and **Windows** (Internet Options). Linux has no system-wide proxy setting, so there the environment variables are the only automatic source.

Note that qBittorrent connections deliberately **bypass** the proxy, since the WebUI
is local/LAN.

## Configuration

State lives at `$XDG_CONFIG_HOME/mikan/config.toml` (falling back to
`~/.config/mikan/config.toml`) and holds:

- **`path_history`** — your last 10 export paths, for autocomplete.
- **`qbt`** — saved qBittorrent profiles (endpoint, username, password).

Because the file can contain qBittorrent passwords, it is written atomically with
owner-only (`0600`) permissions on Unix. If the file ever becomes unparseable it is
moved aside to `config.toml.bak` rather than silently overwritten, so saved profiles
are never lost.

## Exit codes

- `0` — success (or a clean "nothing selected" / "cancelled").
- `1` — completed, but one or more episodes failed to download or add.
- `2` — usage error (missing/invalid arguments).

Failures within a batch are reported per-episode and never abort the whole run.
