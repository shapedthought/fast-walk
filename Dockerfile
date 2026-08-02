# Build fast-walk without installing a Rust toolchain.
#
#     docker build -t fast-walk .
#
# Scan a directory, leaving the results CSVs in the working directory:
#
#     docker run --rm -v /srv/share:/scan:ro -v "$PWD":/out fast-walk -p /scan
#
# Mount a share inside the container instead of on the host. mount(2) needs
# CAP_SYS_ADMIN, which a container does not have by default, and the mount
# commands are the ones in docs/snapshot-scanning.md rather than a second
# interface invented here:
#
#     docker run --rm --cap-add SYS_ADMIN \
#         -v "$PWD":/out -v "$PWD/.smbcred":/creds:ro \
#         --entrypoint bash fast-walk -c '
#             mkdir -p /scan
#             mount -t cifs //fileserver/data /scan -o ro,vers=3.0,credentials=/creds
#             fast-walk -p /scan'
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

# The mount helpers, so a share can be mounted in the container rather than on
# the host. These are the two packages docs/snapshot-scanning.md already names,
# and they are needed for more than convenience: without mount.cifs the
# credentials= option is parsed by nobody and silently dropped, so the mount is
# attempted anonymously and the server rejects it, which reads as an
# authentication problem rather than a missing package. Without mount.nfs,
# mount falls back to the raw system call and reports every option it cannot
# parse as a bad option.
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        cifs-utils \
        nfs-common \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/fast-walk /usr/local/bin/fast-walk

# Results go to the working directory unless --output says otherwise, so this
# is the path to mount when the CSVs are wanted after the container exits.
WORKDIR /out

ENTRYPOINT ["fast-walk"]

# Bare `docker run fast-walk` should explain itself rather than fail on a
# missing --path.
CMD ["--help"]
