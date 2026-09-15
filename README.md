# zerch

A `less`-but-smarter terminal log viewer — search, filters, highlights, bookmarks,
log-level / timestamp awareness, and a time histogram — as a single small native
binary (~2.1 MB, links only the system C library).

This is Command Line Tool written in Rust with the goal of being fast and responsive,
as well as safe and small executable.

Design goals:

- **Small footprint** — one lean binary, few dependencies, easy to package for Debian.
- **Adaptable to scale** — an in-memory index (`mmap` + parallel scan) opens multi-GB
  files near-instantly (a 1.36 GB / ~900k-line file indexes in ~1 s) and searches
  them in well under a second, with bounded memory.
- **Portable views** — the `--view` string captures the full view (filters, highlights,
  bookmarks, revealed ranges, search, selected line) as one compact, shareable token.

## Usage

```
zerch <logfile> [--view <string>]
zerch <logfile> --web       # open in a local browser tab instead of the TUI
zerch -                     # read log from stdin (explicit)
<cmd> | zerch               # read log from stdin (implicit; less-style)
```

`--view` restores filters, highlights, bookmarks, revealed ranges, the search, and
the selected line, so you can save or hand off an exact view as a single string.

When reading from stdin (a pipe or `-`), zerch spools it to a temp file first,
then indexes it as usual — so `kubectl logs foo | zerch`, `zcat huge.gz | zerch`,
etc. work out of the box. The temp file is deleted on exit.

### History

The last 100 terms you enter at the `/` search, `F` filter, and `h` highlight
prompts are remembered across sessions (recall them with `↑`/`↓`), like `less`.
They're stored — newest first, owner-readable only — at:

```
$XDG_STATE_HOME/zerch/history      # or ~/.local/state/zerch/history
```

Set `ZERCH_HISTFILE` to use a different path, or to `-` (or empty) to disable
persistence entirely. All history I/O is best-effort and never blocks the viewer.

### Sessions

Each file's view — its filters, highlights, bookmarks, hidden lines, search, and
the cursor line — is remembered between runs, so reopening `zerch app.log` drops
you back exactly where you left off. Sessions are keyed by the file's canonical
path and stored (private, `0600`) under `$XDG_STATE_HOME/zerch/sessions/`.

- `--view <string>` takes precedence over a saved session.
- `--fresh` opens the file clean, ignoring its saved session (but still saves on exit).
- `ZERCH_NO_SESSION=1` disables saving and restoring entirely.

Piped/stdin input has no session (there's no stable file to key on).

### Web UI (`--web`)

`zerch <file> --web` opens the log in a local browser tab instead of the TUI, reusing
the same engine and view model. It's a fast local viewer with clickable filters
(with per-term counts and AND/OR), highlights, log-level chips, live search with
`n`/`N` navigation, bookmarks and revealed ranges, hide/reveal, a minimap, a
drag-to-filter time histogram, per-line JSON pretty-print and wrap, and a
"copy as shell command" (grep/awk) export — not a remote/multi-user server.

```
zerch app.log --web                 # bind an ephemeral loopback port, open the browser
zerch app.log --web --web-port 8080 # pin the port
kubectl logs pod | zerch --web      # piped input works too (no session)
```

The whole view (filters, highlights, bookmarks, revealed ranges, search) lives in the
URL as the same `--view` token the TUI uses, so **every view is a shareable link** —
copy the URL, or open it in a second tab for an independent view. The server binds
`127.0.0.1` only and prints the URL; if no browser opener is found the URL is still
printed. Stop it with Ctrl+C.

> **Security:** `--web` serves over plain loopback HTTP with no authentication — any
> local process or user on the machine that can reach the port can read the log
> (the same trust boundary as the file itself). It is meant for a single-user local
> machine, not a shared host.

## Keys

| Context | Key | Action |
|---|---|---|
| Move | `j`/`k` or `↓`/`↑` | line down / up |
| | `f`/`b`, `Space`/`PgDn`/`PgUp`, `^F`/`^B` | page down / up |
| | `g` / `G` | top / bottom |
| | `H` / `M` / `L` | cursor to screen top / middle / bottom (no scroll) |
| | `:N` `Enter` · `:$` · `:q` | go to line N · last line · quit |
| Long lines | `←` / `→` | pan half a screen · `N`+`←`/`→` pan N columns · `0` reset pan · `w` toggle in-place wrap |
| Search | `/` | search · `n`/`N` next/prev from cursor · `^R` regex · `^A` case · `↑`/`↓` history · `Esc` clear |
| Highlight | `h` | add a coloured highlight term |
| Filter | `F` add · `o` toggle AND/OR | reduces the view to matching lines (gaps collapse the rest) |
| Bookmarks | `B` toggle · `[` / `]` prev/next | bookmarked lines stay visible when filtered; edit the list inline in the `m` menu (accepts ranges, e.g. `1,3-5,8`) |
| Hide | `x` on a line | hide it (collapses to a gap); reveal the gap or menu `X` to undo |
| Gaps | `Enter`/`+` from top · `-` from bottom | reveal hidden lines a chunk at a time |
| Expand | `Enter` on a line | JSON view, or a wrapped view if the line doesn't fit |
| Histogram | `T` show/hide · `t` re-enter | `←/→` move · `a` anchor · `Enter` apply · `c` clear time range |
| Menu | `m` | toggle/edit/delete filters & highlights; `e` export view string; `g` export grep/awk |
| Field hiding | in the menu | edit `Separator` and `Hide Columns` (e.g. `1,2,5` or `1,3-5,8`) inline (Enter to edit) — chosen columns render as `…` |
| Other | `?` help · `q` quit (confirm) · `^C` quit now | |

## Building

Requires a Rust toolchain (1.80+). No external system libraries are needed — the
binary links only the platform C runtime (`libc`).

```
cargo build --release
```

The optimized binary lands at `target/release/zerch`. **Always use the release
build for large files** — the plain `cargo build` (dev profile) is unoptimized and
its regex is far slower at runtime.

### Linux

1. Install a C linker (Rust shells out to `cc` to link):

   ```
   # Debian / Ubuntu
   sudo apt install build-essential
   # Fedora
   sudo dnf install gcc
   # Alpine
   sudo apk add build-base
   ```

2. Install Rust (if not already), then build:

   ```
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source "$HOME/.cargo/env"
   cargo build --release
   ./target/release/zerch /path/to/logfile
   ```

That's it — there are no `-dev` package dependencies to install (SQLite is not used;
the TUI, search, and regex are all pure Rust). The result is a self-contained binary
depending only on `libc`/`libgcc`; verify with `ldd target/release/zerch`.

### Install on your PATH

```
cargo install --path .        # installs `zerch` into ~/.cargo/bin
```

### Build profiles

| Command | Output | Use for |
|---|---|---|
| `cargo build --release` | `target/release/zerch` | normal use — fast, small, stripped |
| `cargo build --profile release-debug` | `target/release-debug/zerch` | debugging — release speed **+ symbols** for `gdb`/`lldb` |
| `cargo build` | `target/debug/zerch` | quick compiles only — slow at runtime |

## Packaging (Debian / Ubuntu)

The binary links only the system C library, so the `.deb` is tiny and needs no extra
runtime. There are two supported ways to build one (both Linux-only).

### Quick: `cargo-deb`

Uses the `[package.metadata.deb]` section in `Cargo.toml` (binary, man page, docs,
licenses). `Depends:` is derived automatically from the linked libraries.

```
cargo install cargo-deb
cargo deb            # → target/debian/zerch_0.7.4_<arch>.deb
sudo apt install ./target/debian/zerch_*.deb
```

### Native source package: `debian/`

A debhelper source package lives in `debian/` (required at the repo root by
`dpkg-buildpackage`). It builds the binary directly with `cargo` (not `dh-cargo`):

```
sudo apt install devscripts debhelper cargo rustc
dpkg-buildpackage -us -uc -b     # → ../zerch_0.7.4_<arch>.deb
```

`cargo` fetches the crate dependencies during the build, so the build host needs
network access — or vendor them first with `cargo vendor` and a `.cargo/config.toml`
pointing at the vendor dir. Also ensure the toolchain satisfies the crate's
`rust-version`; if your distro's `rustc` is too old, put a rustup toolchain on `PATH`.

Both paths install the binary to `/usr/bin/zerch` and the man page to
`/usr/share/man/man1/zerch.1`.

### Build & test the `.deb` in Docker

`packaging/deb.Dockerfile` builds the package in a clean Debian image and smoke-tests
it (installs into `debian:bookworm-slim`, runs `zerch --help`, checks the man page is
packaged) — handy from a non-Linux host:

```
docker build -f packaging/deb.Dockerfile --target test -t zerch-deb .
# copy the built .deb out:
docker build -f packaging/deb.Dockerfile --target export -o type=local,dest=./dist .
```

The derived runtime deps are just `libc6` and `libgcc-s1`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
