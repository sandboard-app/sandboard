# sandboard board image — API binary + built React UI, pushed as
# quay.io/sandboard-app/sandboard. Distinct from sandbox/Containerfile (agent sandboxes).
#
#   podman build -f Containerfile -t quay.io/sandboard-app/sandboard:latest .
#   make image / make image-push

# ---- UI --------------------------------------------------------------------

FROM docker.io/library/node:26-slim AS ui

WORKDIR /src
COPY web/package.json web/package-lock.json ./web/
RUN npm ci --prefix web
COPY web/ ./web/
RUN npm run build --prefix web

# ---- API -------------------------------------------------------------------

FROM registry.access.redhat.com/ubi9/ubi:latest AS api

USER root

# gcc/make for crates that compile C (ring, sqlx sqlite feature even when we
# only ship postgres in-cluster — the binary still links those features).
# openssl-devel / pkg-config cover any native TLS deps in the graph.
RUN dnf install -y --setopt=install_weak_deps=False \
      gcc gcc-c++ make openssl-devel pkg-config \
 && dnf clean all

ARG RUST_VERSION=1.97.1
ENV RUSTUP_HOME=/opt/rust \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal \
           --default-toolchain "${RUST_VERSION}" \
 && cargo --version

WORKDIR /src
# Cache dependency compile: copy manifests first, stub a fake main, then the
# real sources. A Cargo.toml/Cargo.lock change invalidates this layer; source
# edits alone reuse the dep cache.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && printf 'fn main() {}\n' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/deps/sandboard* target/release/sandboard*

COPY migrations/ ./migrations/
COPY src/ ./src/
COPY llms.txt ./
# include_str! for shipped OpenShell provider YAMLs + llms.txt; sqlx::migrate!
# embeds migrations/ at compile time.
COPY sandbox/openshell/ ./sandbox/openshell/
RUN cargo build --release --locked

# ---- runtime ---------------------------------------------------------------

FROM registry.access.redhat.com/ubi9/ubi-minimal:latest

USER root

# ca-certificates for tonic/reqwest rustls (tls-native-roots / webpki-roots).
RUN microdnf install -y --setopt=install_weak_deps=0 ca-certificates \
 && microdnf clean all \
 && groupadd -r -g 1000 sandboard \
 && useradd -r -u 1000 -g sandboard -d /app -s /sbin/nologin sandboard

WORKDIR /app
COPY --from=api /src/target/release/sandboard /app/sandboard
COPY --from=ui /src/web/dist /app/web/dist

USER sandboard
EXPOSE 8080
ENV SANDBOARD_BIND_ADDR=0.0.0.0 \
    SANDBOARD_PORT=8080

ENTRYPOINT ["/app/sandboard"]
