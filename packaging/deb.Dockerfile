# Build the Debian package in a clean Debian container and smoke-test it by
# installing the resulting .deb into a fresh image.
#
#   docker build -f packaging/deb.Dockerfile -t zerch-deb .
#   docker run --rm zerch-deb            # prints `zerch --help`
#
# To copy the .deb out of the build stage:
#   docker build -f packaging/deb.Dockerfile --target export -o type=local,dest=./dist .

# ---- build the .deb -------------------------------------------------------
FROM rust:slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        debhelper devscripts dpkg-dev build-essential ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# The Rust toolchain comes from the image (rustup), not dpkg, so skip the
# build-dependency check (-d). Crates are fetched from the network at build time.
RUN dpkg-buildpackage -us -uc -b -d
RUN mkdir -p /out && cp ../zerch_*_*.deb /out/

# ---- smoke-test: install into a clean Debian and run it -------------------
FROM debian:bookworm-slim AS test
COPY --from=build /out/*.deb /tmp/
# `apt-get install ./file.deb` resolves and installs the derived Depends.
RUN apt-get update \
    && apt-get install -y --no-install-recommends /tmp/*.deb \
    && rm -rf /var/lib/apt/lists/* \
    && dpkg-deb -I /tmp/*.deb \
    && zerch --help \
    # slim images exclude man pages on install, so assert it's *packaged*:
    && dpkg-deb -c /tmp/*.deb | grep -q 'usr/share/man/man1/zerch.1.gz' \
    && echo "OK: installs cleanly, runs, man page packaged"
CMD ["zerch", "--help"]

# ---- optional: export just the .deb --------------------------------------
FROM scratch AS export
COPY --from=build /out/*.deb /
