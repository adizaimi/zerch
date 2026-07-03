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

```
cargo build --release
```

The optimized binary lands at `target/release/zercher`. Use the release build for
large files — the debug build's regex is much slower.

## Packaging (Debian)

The binary links only the system C library, so a `.deb` is tiny and needs no extra
runtime. With [`cargo-deb`](https://github.com/kornelski/cargo-deb):

```
cargo install cargo-deb
cargo deb            # produces target/debian/zercher_*.deb
```

`cargo deb` derives the `Depends:` line automatically from the linked libraries.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
