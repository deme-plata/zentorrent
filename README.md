<div align="center">

<a href="https://deme-plata.github.io/zentorrent/">
  <img src="docs/swarm.gif" alt="ZenTorrent — a BitTorrent swarm rendered in three.js: your client at the centre, seeders in green, leechers in blue, private-tracker peers in violet, pieces flying in and out" width="100%">
</a>

<sub>▲ The swarm, rendered in <b>three.js</b>. <a href="https://deme-plata.github.io/zentorrent/">Open the live 3D scene →</a> (move your mouse)</sub>

<br><br>

**A fast, ad-free desktop BitTorrent client in Rust.**<br>
No ads. No bundled toolbars. No telemetry. Just the swarm.

<br>

![Rust](https://img.shields.io/badge/Rust-2021-b7410e?style=for-the-badge&logo=rust&logoColor=white)
![egui](https://img.shields.io/badge/UI-egui_0.35-4682dc?style=for-the-badge)
![librqbit](https://img.shields.io/badge/engine-librqbit_9-3caa5a?style=for-the-badge)
![Platforms](https://img.shields.io/badge/Linux_%7C_Windows-x64-555?style=for-the-badge)
![Ads](https://img.shields.io/badge/ads-zero-aa78e6?style=for-the-badge)
![Built with Flux](https://img.shields.io/badge/built_with-%E2%9A%A1_Flux-ffd25a?style=for-the-badge&labelColor=222)

</div>

---

## Why another torrent client?

Most torrent clients still look like it is 2008, and the popular ones wrap the
download in ads and bundled offers. ZenTorrent is one small native binary: an
[egui](https://github.com/emilk/egui) window over the
[librqbit](https://github.com/ikatson/rqbit) engine. It starts instantly, and
everything in it exists because someone needed it on a real private tracker.

## ✨ What's in it

### The sidebar — a navigator, not a list of folders *(new in 0.6)*

<img src="docs/sidebar.png" alt="ZenTorrent's sidebar with live status counts, trackers grouped by site and user labels" width="100%">

| | |
|---|---|
| **Live status facets** | All · Downloading · Seeding · Completed · Paused · **Active** (moving bytes now) · **Stalled** (no one has your missing pieces) · Errored |
| **Faceted counts** | Status, tracker and label combine. Every number is computed with the *other* filters applied, so it always tells you what you'd see if you clicked it. |
| **Trackers, by site** | `tracker.torrentleech.org` and `tleechreload.org` are both **TorrentLeech**; `udp://tracker.opentrackr.org` is **OpenTrackr**. Private trackers show 🔒 and your **ratio on that tracker**. Announce URLs carry your passkey, so only the host is ever shown. |
| **Needs seeding 🔒** | A smart view: private torrents that are finished but still below your ratio goal. Leave these running and your account stays healthy. |
| **Labels** | Make your own (Film, Linux, Keepers…). Tag torrents from the row's **Labels** menu, or… |
| **Drag & drop** | …drag a torrent's name onto a label to tag it, onto **Paused** to pause it, onto **Downloading/Seeding** to resume it. |
| **Search** | `Ctrl+F`. Matches names, labels and tracker names. |
| **Speed sparkline** | The last 90 seconds of ⬇/⬆ at a glance. |

### Details: one torrent, up close *(new in 0.7)*

<img src="docs/details.png" alt="ZenTorrent's Details panel: a live download/upload speed graph above Status, Details, Files, Peers and Settings tabs" width="100%">

Press **Details** on any torrent:

| | |
|---|---|
| **Live speed graph** | 5 minutes of ⬇/⬆ as gradient areas on a scale that always lands on round numbers. Hover for the exact speed at any second. |
| **Status** | Downloaded, lifetime upload and ratio, speed, time left, peers, seeding time, pieces verified, average piece time. |
| **Details** | Info hash and magnet link (one click to copy, hash and name only), save folder, piece size, and trackers shown as host only. Your passkey never appears. |
| **Files** | Choose which files to download, with per-file progress and a **Largest only** button for the one film or ISO in a pack. |
| **Peers** | Everyone you're trading with: address, client (qBittorrent, Deluge, µTorrent…), TCP/uTP, and what you got from and sent to each. |
| **Settings** | **This torrent's own download/upload limits**, applied instantly with no restart or re-check. Its own seed goal, labels, pause/resume. Plus limits for **all torrents**. Everything is remembered across restarts. |

### Everything else

- **Seeding & ratio ledger.** Lifetime upload per torrent survives restarts (librqbit's own counter resets every run). Stop seeding at a ratio or after N hours.
- **RSS / Atom / Torznab feeds** with regex auto-download. The first read only primes the rule, so you never get a flood of the backlog.
- **Private-tracker log-in.** One browser-like identity for feed, log-in and download, because TBDev sites bind the session to the User-Agent. The password is used once and never stored.
- **Posters and ratings** in feeds: IMDb, Rotten Tomatoes and Metascore via the OMDb API (HTTPS, cached, rate-limited). No page scraping.
- **Resumes after restart.** Downloads and seeding pick up where they stopped.
- **Signed self-update.** An Ed25519-signed manifest plus a BLAKE3 + size check before `self_replace`. A tampered binary is refused.

## 📦 Install

Grab a build from **[GitHub Releases](https://github.com/deme-plata/zentorrent/releases/latest)**. After that, the **Update** button in the top-right corner keeps you current:

| Platform | GitHub Releases | Mirror |
|---|---|---|
| Windows x64 | [`zentorrent-0.7.0-windows-x64.exe`](https://github.com/deme-plata/zentorrent/releases/download/v0.7.0/zentorrent-0.7.0-windows-x64.exe) | [quillon.xyz](https://quillon.xyz/downloads/zentorrent-0.7.0-windows-x64.exe) |
| Linux x64 | [`zentorrent-0.7.0-linux-x64`](https://github.com/deme-plata/zentorrent/releases/download/v0.7.0/zentorrent-0.7.0-linux-x64) | [quillon.xyz](https://quillon.xyz/downloads/zentorrent-0.7.0-linux-x64) |

Both are byte-identical to what the in-app updater installs. Their BLAKE3 hashes are in the Ed25519-signed [`zentorrent-latest.json`](https://quillon.xyz/downloads/zentorrent-latest.json).

```bash
chmod +x zentorrent-0.7.0-linux-x64 && ./zentorrent-0.7.0-linux-x64
```

## 🛠 Build from source

```bash
git clone https://github.com/deme-plata/zentorrent && cd zentorrent
cargo build --release          # plain cargo works fine
```

We build it with [**Flux**](https://github.com/deme-plata/flux) (`fluxc build --release`), the
same toolchain behind [SigilGraph](https://github.com/deme-plata/sigilgraph). It adds a
content-addressed compile cache, a per-build compile-cost verdict, and webhooks on build/test
events. Every release is stamped with a [`flux-rev`](https://github.com/deme-plata/flux)
BLAKE3 content id, so anyone can re-snapshot the tree and check it.

### Headless modes (handy for testing and servers)

| command | does |
|---|---|
| `zentorrent --cli <magnet / url / file> [dir]` | download with a progress line, no window |
| `zentorrent --feed <url>` | print what the feed parser sees (passkey masked) |
| `zentorrent --fetch <torrent-url> [cookie]` | fetch one .torrent the way the app does |
| `zentorrent --login <feed-url> <user> <pass>` | tracker log-in with proof |
| `zentorrent --update` | check the signed channel, install if newer |

## 🧭 How it fits together

```mermaid
flowchart LR
    subgraph UI["egui window"]
        SB["sidebar.rs<br/>status · trackers · labels<br/>faceted filter + drag & drop"]
        LIST["Downloads / Seeding / RSS"]
    end
    SB -- filter --> LIST
    LIST -- add / pause / resume --> ENG["librqbit Session<br/>DHT · trackers · peers<br/>piece verification"]
    ENG -- stats · trackers --> SB
    RSS["rss.rs<br/>RSS / Atom / Torznab<br/>log-in + cookie"] --> ENG
    META["meta.rs<br/>OMDb ratings + posters"] --> LIST
    SEED["seed.rs<br/>ratio ledger + labels<br/>ratio.json"] <--> SB
    UPD["update.rs<br/>Ed25519 manifest<br/>BLAKE3 + size"] -.-> UI
```

| file | job |
|---|---|
| `src/main.rs` | the window, tabs, transfer list, CLI modes |
| `src/sidebar.rs` | the faceted navigator: statuses, tracker → site grouping, labels, drag & drop, sparkline |
| `src/details.rs` | the Details panel: speed graph, Status / Details / Files / Peers / Settings tabs |
| `vendor/librqbit` | librqbit 9.0.1 plus **one** added getter, so a torrent's speed limits can change while it runs ([why](vendor/README-zentorrent.md)) |
| `src/rss.rs` | feed parser (RSS 2.0 / Atom / Torznab), `.torrent` fetch + validation, tracker log-in |
| `src/meta.rs` | release-name → title/year guesser, OMDb lookups, poster cache, daily budget |
| `src/seed.rs` | lifetime ratio ledger, seed goals, labels |
| `src/update.rs` | signed self-updater |

## 🔐 Privacy & security, in plain words

- **Your passkey never appears on screen.** Feed URLs are masked, the sidebar shows tracker *hosts* only, and every error message strips the URL.
- `feeds.json` is written `0600`. Cookies are session credentials and are treated that way.
- **Your password is not stored.** ZenTorrent logs in once and keeps only the session cookie.
- Private torrents never touch DHT or LSD (the `private` flag is honoured by librqbit).
- All HTTP goes over **rustls**. Posters are HTTPS-only and size-capped.
- Updates are refused unless the manifest's **Ed25519 signature** verifies against a key pinned in the binary, *and* the download matches its BLAKE3 hash and size.

## 🗺 Roadmap

- [x] Sidebar: status / tracker / label facets, drag & drop, per-tracker ratio *(0.6)*
- [x] Details panel: speed graph, files, peers, per-torrent and global speed limits *(0.7)*
- [ ] RSS search (genre, seeders, activity) and a history that remembers every feed item
- [ ] Opt-in, anonymous usage statistics *(0.8)*
- [ ] **Flux MoE assistant tab**: ask about your feeds and history, let it sort a music folder by genre
- [ ] **IronTunnel**: a built-in Rust VPN tunnel to bind torrent traffic to *(in progress)*
- [ ] Built-in audio/video player
- [ ] Speed schedules · tray icon · macOS build

## License

Dual-licensed under **MIT OR Apache-2.0**, at your option.

<div align="center"><sub>⚡ Made with care on the Flux toolchain · <a href="https://deme-plata.github.io/zentorrent/">live 3D swarm</a></sub></div>
