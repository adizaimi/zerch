# zercher

A `less`-but-smarter terminal log viewer — search, filters, highlights, bookmarks,
log-level / timestamp awareness, and a time histogram — as a single small native
binary (~1.8 MB, links only the system C library).

This is a Rust rewrite of the terminal viewer that previously lived inside the
Node/TypeScript `zercher` project (which needed a ~265 MB Node + `node_modules`
install). Design goals:

- **Small footprint** — one lean binary, few dependencies, easy to package for Debian.
- **Adaptable to scale** — an in-memory index (`mmap` + parallel scan) opens multi-GB
  files near-instantly (a 1.36 GB / ~900k-line file indexes in ~1 s) and searches
  them in well under a second, with bounded memory.
- **Interoperable** — the `--view` string is byte-compatible with the zercher web
  app's shareable URL, so a saved view moves between the web UI and the CLI.

## Usage

```
zercher <logfile> [--view <string>]
```

`--view` restores filters, highlights, bookmarks, revealed ranges, the search, and
the selected line — e.g. a string copied from the web app's address bar.

## Keys

| Context | Key | Action |
|---|---|---|
| Move | `j`/`k` or `↓`/`↑` | line down / up |
| | `f`/`b`, `Space`/`PgDn`/`PgUp`, `^F`/`^B` | page down / up |
| | `g` / `G` | top / bottom |
| Long lines | `←` / `→` | pan · `0` reset pan · `w` toggle in-place wrap |
| Search | `/` | search · `n`/`N` next/prev · `^R` regex · `^A` case · `↑`/`↓` history · `Esc` clear |
| Highlight | `h` | add a coloured highlight term |
| Filter | `F` add · `o` toggle AND/OR | reduces the view to matching lines (gaps collapse the rest) |
| Bookmarks | `B` toggle · `[` / `]` prev/next | bookmarked lines stay visible when filtered |
| Gaps | `Enter`/`+` from top · `-` from bottom | reveal hidden lines a chunk at a time |
| Expand | `Enter` on a line | JSON view, or a wrapped view if the line doesn't fit |
| Histogram | `T` show/hide · `t` re-enter | `←/→` move · `a` anchor · `Enter` apply · `c` clear time range |
| Menu | `m` | toggle/edit/delete filters & highlights; `e` export view string; `g` export grep/awk |
| Other | `?` help · `q` quit (confirm) · `^C` quit now | |

## Building

Requires a Rust toolchain (1.85+). No external system libraries are needed — the
binary links only the platform C runtime (`libc`).

```
cargo build --release
```

The optimized binary lands at `target/release/zercher`. **Always use the release
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
   ./target/release/zercher /path/to/logfile
   ```

That's it — there are no `-dev` package dependencies to install (SQLite is not used;
the TUI, search, and regex are all pure Rust). The result is a self-contained binary
depending only on `libc`/`libgcc`; verify with `ldd target/release/zercher`.

### Install on your PATH

```
cargo install --path .        # installs `zercher` into ~/.cargo/bin
```

### Build profiles

| Command | Output | Use for |
|---|---|---|
| `cargo build --release` | `target/release/zercher` | normal use — fast, small, stripped |
| `cargo build --profile release-debug` | `target/release-debug/zercher` | debugging — release speed **+ symbols** for `gdb`/`lldb` |
| `cargo build` | `target/debug/zercher` | quick compiles only — slow at runtime |

## Packaging (Debian / Ubuntu)

The binary links only the system C library, so the `.deb` is tiny and needs no extra
runtime. There are two supported ways to build one (both Linux-only).

### Quick: `cargo-deb`

Uses the `[package.metadata.deb]` section in `Cargo.toml` (binary, man page, docs,
licenses). `Depends:` is derived automatically from the linked libraries.

```
cargo install cargo-deb
cargo deb            # → target/debian/zercher_0.1.0_<arch>.deb
sudo apt install ./target/debian/zercher_*.deb
```

### Native source package: `debian/`

A standard debhelper + `dh-cargo` source package lives in `debian/` (required at the
repo root by `dpkg-buildpackage`):

```
sudo apt install devscripts debhelper dh-cargo
dpkg-buildpackage -us -uc -b     # → ../zercher_0.1.0_<arch>.deb
```

Note: a Debian build runs offline, so the crate dependencies must be available —
either the Rust crates packaged in your Debian release, or vendored first with
`cargo vendor` (plus a `.cargo/config.toml` pointing at the vendor dir).

Both paths install the binary to `/usr/bin/zercher` and the man page to
`/usr/share/man/man1/zercher.1`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
