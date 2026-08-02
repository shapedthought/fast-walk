# Build fast-walk without installing a Rust toolchain.
#
#     docker build -t fast-walk .
#
# Scan a directory, leaving the results CSVs in the working directory:
#
#     docker run --rm -v /srv/share:/scan:ro -v "$PWD":/out fast-walk -p /scan
#
# Or take the binary and leave the image behind:
#
#     docker create --name fw fast-walk
#     docker cp fw:/usr/local/bin/fast-walk .
#     docker rm fw

# Pinned rather than :latest, so that a build repeated in a year uses the
# toolchain this was tested against instead of whatever is current then.
FROM rust:1.94-slim-bookworm AS build

WORKDIR /src

# The dependencies resolve from the manifest alone, so building them against a
# stub source puts them in a layer that editing anything under src/ does not
# invalidate. That is most of a cold build; the crate itself is three files.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && : > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# Cargo decides what to rebuild from modification times, and the stubs above
# were written after the real sources were last touched on the host.
RUN touch src/*.rs && cargo build --release --locked

# Nothing from the toolchain is needed to run a scan, so the builder and its
# registry are left behind rather than shipped.
FROM debian:bookworm-slim

COPY --from=build /src/target/release/fast-walk /usr/local/bin/fast-walk

# Results go to the working directory unless --output says otherwise, so this
# is the path to mount when the CSVs are wanted after the container exits.
WORKDIR /out

ENTRYPOINT ["fast-walk"]

# Bare `docker run fast-walk` should explain itself rather than fail on a
# missing --path.
CMD ["--help"]
