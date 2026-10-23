# joinedcontext-platform: one image, six binaries (context-gateway = entrypoint; jcctl, jc-agent-proxy,
# jc-functions, jc-assistant and jc-token-sidecar = `docker run … <binary>`, which is how the agent runner,
# the functions component, the assistant's crawl worker and the pipelines' token service run them).
# Published by .github/workflows/image.yml as ghcr.io/marek-mraz-jc/joinedcontext-platform, pinned by digest in the deployment.
# Debian 13 throughout, every base by digest: the build stage's glibc is the runtime's (T-4126).
FROM rust:1.97-slim-trixie@sha256:8e8cf8f7fd54a2d23d5a743b3a03f56e26b6c774276c33fa0595111704ebb15c AS build
WORKDIR /src
# g++: the tokenizer behind the assistant's embedder links C++ (esaxx, oniguruma; T-3053).
# libclang-dev: jc-wasm-host reads an App's SQL with libpg_query, whose bindings bindgen makes (T-3364).
# make: the gateway's allocator, jemalloc, is built from its C source (T-4782).
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev curl ca-certificates g++ libclang-dev make && rm -rf /var/lib/apt/lists/*
# The PDFium the assistant crate links, pinned and checked (scripts/ci/pdfium.sh, T-3052).
COPY scripts/ci/pdfium.sh /tmp/pdfium.sh
RUN sh /tmp/pdfium.sh /opt/pdfium
ENV KREUZBERG_PDFIUM_PREBUILT=/opt/pdfium
# The assistant's embedding model and the ONNX Runtime it loads, pinned and checked (T-3053);
# only the final image's jc-assistant reads them.
COPY scripts/ci/e5-small.sh scripts/ci/onnxruntime.sh /tmp/
RUN sh /tmp/e5-small.sh /opt/e5-small && sh /tmp/onnxruntime.sh /opt/onnxruntime
# dependency layer first so source edits do not rebuild the world; every target a Cargo.toml names
# gets a stub, or cargo refuses the manifest (scripts/ci/check-dockerfile-members.py)
COPY Cargo.toml Cargo.lock ./
COPY crates/jc-core/Cargo.toml crates/jc-core/Cargo.toml
COPY crates/jc-dcat/Cargo.toml crates/jc-dcat/Cargo.toml
COPY crates/jc-manifest/Cargo.toml crates/jc-manifest/Cargo.toml
COPY crates/jc-relationships/Cargo.toml crates/jc-relationships/Cargo.toml
COPY crates/jc-tabular/Cargo.toml crates/jc-tabular/Cargo.toml
COPY crates/context-gateway/Cargo.toml crates/context-gateway/Cargo.toml
COPY crates/jcctl/Cargo.toml crates/jcctl/Cargo.toml
COPY crates/agent-proxy/Cargo.toml crates/agent-proxy/Cargo.toml
COPY crates/functions/Cargo.toml crates/functions/Cargo.toml
COPY crates/assistant/Cargo.toml crates/assistant/Cargo.toml
COPY crates/token-sidecar/Cargo.toml crates/token-sidecar/Cargo.toml
COPY crates/wasm-host/Cargo.toml crates/wasm-host/Cargo.toml
COPY crates/app-sdk/Cargo.toml crates/app-sdk/Cargo.toml
COPY crates/camera-sim/Cargo.toml crates/camera-sim/Cargo.toml
RUN mkdir -p crates/jc-core/src crates/jc-dcat/src crates/jc-manifest/src crates/jc-relationships/src crates/jc-tabular/src crates/context-gateway/src crates/jcctl/src crates/agent-proxy/src crates/functions/src crates/assistant/src crates/token-sidecar/src crates/wasm-host/src crates/app-sdk/src crates/camera-sim/src \
 && echo 'pub fn _dep_cache() {}' > crates/jc-core/src/lib.rs \
 && echo 'pub fn _dep_cache() {}' > crates/jc-dcat/src/lib.rs \
 && echo 'pub fn _dep_cache() {}' > crates/jc-manifest/src/lib.rs \
 && echo 'pub fn _dep_cache() {}' > crates/jc-relationships/src/lib.rs \
 && echo 'pub fn _dep_cache() {}' > crates/jc-tabular/src/lib.rs \
 && echo 'pub fn _dep_cache() {}' > crates/context-gateway/src/lib.rs \
 && echo 'fn main() {}' > crates/context-gateway/src/main.rs \
 && echo 'fn main() {}' > crates/jcctl/src/main.rs \
 && echo 'pub fn _dep_cache() {}' > crates/agent-proxy/src/lib.rs \
 && echo 'fn main() {}' > crates/agent-proxy/src/main.rs \
 && echo 'pub fn _dep_cache() {}' > crates/functions/src/lib.rs \
 && echo 'fn main() {}' > crates/functions/src/main.rs \
 && echo 'pub fn _dep_cache() {}' > crates/assistant/src/lib.rs \
 && echo 'pub fn _dep_cache() {}' > crates/token-sidecar/src/lib.rs \
 && echo 'fn main() {}' > crates/token-sidecar/src/main.rs \
 && echo 'pub fn _dep_cache() {}' > crates/wasm-host/src/lib.rs \
 && echo 'fn main() {}' > crates/wasm-host/src/main.rs \
 && echo 'pub fn _dep_cache() {}' > crates/app-sdk/src/lib.rs \
 && echo 'fn main() {}' > crates/camera-sim/src/main.rs \
 && mkdir -p crates/assistant/src/bin && echo 'fn main() {}' > crates/assistant/src/bin/jc-assistant.rs \
 && mkdir -p crates/context-gateway/benches && echo 'fn main() {}' > crates/context-gateway/benches/pdp_decision.rs \
 && cargo build --release --locked --workspace --exclude assistant --features context-gateway/jemalloc \
 && cargo build --release --locked -p assistant && rm -rf crates/*/src crates/*/benches
COPY . .
# Two invocations, so kreuzberg's serde_json `preserve_order` stays in the assistant and never
# reorders what the gateway and jcctl write (T-3206).
RUN touch crates/*/src/*.rs && cargo build --release --locked --workspace --exclude assistant --features context-gateway/jemalloc \
 && cargo build --release --locked -p assistant \
 && strip target/release/context-gateway target/release/jcctl target/release/jc-agent-proxy target/release/jc-functions target/release/jc-assistant target/release/jc-token-sidecar target/release/jc-camera-sim

# `jcctl checkouts`, the sidecar that keeps every registered project checked out at its ref
# for the gateway (CC-86, T-2646), runs git, which the distroless image does not carry. Its
# own image, built as `--target checkouts`, so git never enters the gateway's.
FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS checkouts
# APT_REFRESH: the UTC day, set by image.yml, so the apt layer below leaves the build cache once a
# day and picks up the Debian security fixes released since (vulnerabilities lane, T-3834).
ARG APT_REFRESH
RUN apt-get update && apt-get upgrade -y && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/* && useradd --uid 65532 --no-create-home --shell /usr/sbin/nologin nonroot
COPY --from=build /src/target/release/jcctl /usr/local/bin/jcctl
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/jcctl", "checkouts"]

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2
COPY --from=build /src/target/release/context-gateway /usr/local/bin/context-gateway
COPY --from=build /src/target/release/jcctl /usr/local/bin/jcctl
COPY --from=build /src/target/release/jc-agent-proxy /usr/local/bin/jc-agent-proxy
COPY --from=build /src/target/release/jc-functions /usr/local/bin/jc-functions
COPY --from=build /src/target/release/jc-token-sidecar /usr/local/bin/jc-token-sidecar
# The camera simulator of the camera-over-mqtt Blueprint (T-3838): `docker run … jc-camera-sim publish`.
COPY --from=build /src/target/release/jc-camera-sim /usr/local/bin/jc-camera-sim
# The shared host of server WASM Apps (ADR-N-044): a shard runs this binary with `command`.
COPY --from=build /src/target/release/jc-wasm-host /usr/local/bin/jc-wasm-host
# The knowledge assistant's crawl worker (Architecture/22, T-3052); its Deployment names it as the
# command, as the agent proxy's does.
COPY --from=build /src/target/release/jc-assistant /usr/local/bin/jc-assistant
COPY --from=build /opt/e5-small /opt/e5-small
COPY --from=build /opt/onnxruntime/libonnxruntime.so /usr/local/lib/libonnxruntime.so
ENV JC_ASSISTANT_MODEL_DIR=/opt/e5-small ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so
# The gateway's jemalloc (T-4782): a background thread purges freed pages after a second, so the
# memory of a load comes back when it ends; four arenas, whatever the node's core count. Measured
# in context-gateway's tests/in_flight_tests.rs, which reads this line.
ENV _RJEM_MALLOC_CONF=background_thread:true,narenas:4,dirty_decay_ms:1000,muzzy_decay_ms:0
USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/context-gateway"]
