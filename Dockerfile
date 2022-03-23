FROM rust:bullseye as builder

RUN apt-get update && apt-get install -y lld
# First we build just our dependencies so they are cacheable
WORKDIR /usr/src
RUN USER=root cargo new --bin homelab-dns
WORKDIR /usr/src/homelab-dns
COPY Cargo.toml Cargo.lock ./
RUN CARGO_INCREMENTAL=0 CARGO_PROFILE_RELEASE_LTO=thin RUSTFLAGS="-C link-arg=-fuse-ld=lld -C link-arg=-Wl,--compress-debug-sections=zlib -C force-frame-pointers=yes" cargo build --release
RUN rm -rf src
# Now we build our actual application
COPY src ./src
RUN touch src/main.rs
RUN CARGO_INCREMENTAL=0 CARGO_PROFILE_RELEASE_LTO=thin RUSTFLAGS="-C link-arg=-fuse-ld=lld -C link-arg=-Wl,--compress-debug-sections=zlib -C force-frame-pointers=yes" cargo build --release

FROM debian:bullseye-slim
ARG APP=/usr/src/app
# We bloat our package a bit by adding dnsutils, but we are running a DNS server so
# having it around is useful
RUN apt-get update && apt-get install -y ca-certificates tzdata dnsutils && rm -rf /var/lib/apt/lists/*
ENV TZ=Etc/UTC APP_USER=appuser
RUN groupadd $APP_USER && useradd -g $APP_USER $APP_USER && mkdir -p ${APP}
COPY --from=builder /usr/src/homelab-dns/target/release/homelab-dns ${APP}/homelab-dns
RUN chown -R $APP_USER:$APP_USER ${APP}
USER $APP_USER
WORKDIR ${APP}
CMD "/usr/src/app/homelab-dns"