# zercher

A `less`-but-smarter terminal log viewer — search, filters, highlights, bookmarks,
and log-level / timestamp awareness — as a single small native binary.

This is a Rust rewrite of the terminal viewer that previously lived inside the
Node/TypeScript `zercher` project. Goals:

- **Small footprint** — one lean binary, few dependencies, easy to package for Debian.
- **Adaptable to scale** — a tiered backend: an always-on in-memory index (mmap +
  parallel scan) that opens multi-GB files instantly, plus an optional persistent
  SQLite/FTS5 trigram index for very large files with heavy repeated searching.
- **Interoperable** — the `--view` string is byte-compatible with the zercher web
  app's shareable URL, so a saved view moves between the web UI and the CLI.

## Status

Early, built incrementally. See the phase roadmap in the project plan.

## Usage

```
zercher <logfile> [--view <string>]
```

Press `?` inside the viewer for the interactive keybinding cheat-sheet.

## Building

```
cargo build --release
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
