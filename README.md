<p align="center">
  <img src="assets/iFetch-logo.png" alt="iFetch" width="180">
</p>

<h1 align="center">iFetch</h1>

<p align="center">
  A fast, lightweight MangaKatana downloader and self-hosted manga archive server written in Rust.
</p>

---

## Overview

**iFetch** downloads manga series and chapters from MangaKatana directly into `.cbz` archives with corresponding `ComicInfo.xml` metadata.

You can run it as an interactive CLI tool or as a lightweight HTTP server. When running in server mode, it functions as a backend for reader apps like [Mihon](https://mihon.app) / Tachiyomi through the [ifetch-extension](https://github.com/FelixSiegel/ifetch-extension).

## Features

- **CLI & Server**: Download interactively from your terminal or run an HTTP backend for reader apps.
- **Mihon Integration**: Browse, read, and trigger downloads using the [ifetch-extension](https://github.com/FelixSiegel/ifetch-extension).
- **CBZ & Metadata**: Packages chapters into `.cbz` files with embedded `ComicInfo.xml` metadata.
- **Fast Reading & Caching**: In-memory caching and prefetching for instant page turns.
- **Automatic Updates**: Periodic background checks for new chapter releases.
- **Lightweight**: Low resource footprint (~10-15 MB RAM baseline).
- **Discord Webhooks**: Optional notifications for downloads and library updates.

## Installation

### Docker

```bash
docker run -d \
  --name ifetch \
  -p 8080:8080 \
  -v /path/to/downloads:/app/downloads \
  -v /path/to/config:/app/config \
  -e DISCORD_WEBHOOK_URL="https://discord.com/api/webhooks/..." \
  ghcr.io/felixsiegel/ifetch:latest
```

#### Docker Compose

```yaml
services:
  ifetch:
    image: ghcr.io/felixsiegel/ifetch:latest
    container_name: ifetch
    restart: unless-stopped
    ports:
      - "8080:8080"
    volumes:
      - ./downloads:/app/downloads
      - ./config:/app/config
    environment:
      - IFETCH_PORT=8080
      - IFETCH_THREADS=2
      - IFETCH_CRON_HOURS=12
      - IFETCH_CACHE_TTL_MINS=5
      - DISCORD_WEBHOOK_URL=
```

### Building from Source

Requires Rust (1.85+).

To just install, you can use:

```bash
cargo install --git https://github.com/FelixSiegel/ifetch
```

or if you want the full project source:

```bash
git clone https://github.com/FelixSiegel/ifetch.git
cd ifetch
cargo build --release
```

The compiled binary will be located at `target/release/ifetch`.

## Usage

### CLI Mode

```bash
# Interactive prompt (search manga and select chapters)
ifetch

# Search directly and download all chapters
ifetch "One Piece" --chapters all

# Download specific chapter ranges
ifetch "https://mangakatana.com/manga/..." --chapters 1-20

# Multi-threaded download
ifetch "Berserk" -t 4

# List available chapters without downloading
ifetch "Chainsaw Man" --list
```

### Server Mode

```bash
# Run the HTTP server on port 8080
ifetch --server --port 8080 --output ./downloads --config ./config
```

> [!NOTE] 
> REST API endpoint documentation and a runnable [Bruno](https://usebruno.com) collection are available in [`docs/api/`](docs/api).

### Configuration & Environment Variables

| Variable                | CLI Flag          | Default     | Description                                                     |
| :---------------------- | :---------------- | :---------- | :-------------------------------------------------------------- |
| `IFETCH_SERVER`         | `--server`        | `false`     | Run as an HTTP server                                           |
| `IFETCH_PORT`           | `-p`, `--port`    | `8080`      | Port for the HTTP server                                        |
| `IFETCH_OUTPUT`         | `-o`, `--output`  | `downloads` | Directory for manga archives                                    |
| `IFETCH_CONFIG`         | `-C`, `--config`  | `config`    | Directory for SQLite DB and config                              |
| `IFETCH_THREADS`        | `-t`, `--threads` | `1`         | Concurrent chapter download threads                             |
| `IFETCH_VERIFY`         | `--verify`        | `false`     | Check chapters for updates and re-download if necessary         |
| `IFETCH_CRON_HOURS`     | —                 | `12`        | Interval in hours for automatic chapter checks (`0` to disable) |
| `IFETCH_CACHE_TTL_MINS` | —                 | `5`         | In-memory TTL in minutes for upstream scraped manga metadata    |
| `DISCORD_WEBHOOK_URL`   | —                 | `None`      | Optional webhook URL for Discord alerts                         |

## Mihon / Tachiyomi Extension

If you run iFetch in server mode, you can connect your mobile reader app using the official extension: **[ifetch-extension](https://github.com/FelixSiegel/ifetch-extension)**

Once installed, point the extension's address setting to your iFetch server URL (e.g., `http://your-server-ip:8080`). You can browse, read cached chapters, and trigger background downloads directly from your mobile device.

## Disclaimer

This software is intended strictly for personal use, offline reading, and archiving purposes. **iFetch** does not host, store, or own any copyrighted materials; all metadata and images are retrieved from third-party sources. Please support the original authors, publishers, and official release platforms.
