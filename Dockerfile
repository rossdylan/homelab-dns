FROM rust:buster as builder

WORKDIR /usr/src
RUN USER=root cargo new --bin homelab-dns
WORKDIR /usr/src/homelab-dns
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release
RUN rm -rf src
COPY src ./src
RUN touch src/main.rs
RUN cargo build --release

FROM debian:buster-slim
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