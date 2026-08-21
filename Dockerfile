# Multi-arch static build (issue #19). `rust:alpine`'s default target is
# already `*-unknown-linux-musl` for whatever platform buildx selects (via
# `--platform linux/amd64,linux/arm64`) -- musl targets link libc
# statically by default, so no `--target`/cross-linker setup is needed:
# each platform's build stage just compiles natively for itself, under
# QEMU emulation when that platform isn't the host's own.
FROM rust:alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /src
COPY . .

RUN cargo build --release -p volamos && \
    cp target/release/volamos /volamos

# distroless static + nonroot: no libc at all (matching the fully static
# musl binary above), no shell, runs as an unprivileged user by default --
# see docs/plan.md's Phase 6 section for why this over plain `scratch`.
FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=builder /volamos /usr/local/bin/volamos
# The project's own fixture corpus (small, hand-assembled AmigaOS test
# programs) -- lets `docker run volamos /fixtures/echoargs ...` work out
# of the box without a volume mount, and is what CI's behavioral
# validation step below runs against. Real (licensed) Amiga toolchains
# like SAS/C are never bundled here -- see fixtures/README.md.
COPY fixtures/ /fixtures/

ENTRYPOINT ["/usr/local/bin/volamos"]
