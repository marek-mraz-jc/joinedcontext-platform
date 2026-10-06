# joinedcontext-platform: one image, four binaries (context-gateway = entrypoint; jcctl, jc-agent-proxy
# and jc-functions = `docker run … <binary>`, which is how the agent runner and functions components run them).
# Published by .github/workflows/image.yml as ghcr.io/marek-mraz-jc/joinedcontext-platform, pinned by digest in the deployment.
FROM rust:1.97-slim-bookworm AS build
WORKDIR /src
# g++: the tokenizer behind the assistant's embedder links C++ (esaxx, oniguruma; T-3053).
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev curl ca-certificates g++ && rm -rf /var/lib/apt/lists/*
# The PDFium the assistant crate links, pinned and checked (scripts/ci/pdfium.sh, T-3052).
COPY scripts/ci/pdfium.sh /tmp/pdfium.sh
RUN sh /tmp/pdfium.sh /opt/pdfium
ENV KREUZBERG_PDFIUM_PREBUILT=/opt/pdfium
# The assistant's embedding model and the ONNX Runtime it loads, pinned and checked (T-3053);
# only the final image's jc-assistant reads them.
COPY scripts/ci/e5-small.sh scripts/ci/onnxruntime.sh /tmp/
RUN sh /tmp/e5-small.sh /opt/e5-small && sh /tmp/onnxruntime.sh /opt/onnxruntime
# dependency layer first so source edits do not rebuild the world
COPY Cargo.toml Cargo.lock ./
COPY crates/jc-core/Cargo.toml crates/jc-core/Cargo.toml
COPY crates/context-gateway/Cargo.toml crates/context-gateway/Cargo.toml
COPY crates/jcctl/Cargo.toml crates/jcctl/Cargo.toml
COPY crates/agent-proxy/Cargo.toml crates/agent-proxy/Cargo.toml
COPY crates/functions/Cargo.toml crates/functions/Cargo.toml
COPY crates/assistant/Cargo.toml crates/assistant/Cargo.toml
RUN mkdir -p crates/jc-core/src crates/context-gateway/src crates/jcctl/src crates/agent-proxy/src crates/functions/src crates/assistant/src \
 && echo 'pub fn _dep_cache() {}' > crates/jc-core/src/lib.rs \
 && echo 'pub fn _dep_cache() {}' > crates/context-gateway/src/lib.rs \
 && echo 'fn main() {}' > crates/context-gateway/src/main.rs \
 && echo 'fn main() {}' > crates/jcctl/src/main.rs \
 && echo 'pub fn _dep_cache() {}' > crates/agent-proxy/src/lib.rs \
 && echo 'fn main() {}' > crates/agent-proxy/src/main.rs \
 && echo 'pub fn _dep_cache() {}' > crates/functions/src/lib.rs \
 && echo 'fn main() {}' > crates/functions/src/main.rs \
 && echo 'pub fn _dep_cache() {}' > crates/assistant/src/lib.rs \
 && mkdir -p crates/assistant/src/bin && echo 'fn main() {}' > crates/assistant/src/bin/jc-assistant.rs \
 && cargo build --release --locked --workspace && rm -rf crates/*/src
COPY . .
RUN touch crates/*/src/*.rs && cargo build --release --locked --workspace \
 && strip target/release/context-gateway target/release/jcctl target/release/jc-agent-proxy target/release/jc-functions target/release/jc-assistant

# `jcctl checkouts`, the sidecar that keeps every registered project checked out at its ref
# for the gateway (CC-86, T-2646), runs git, which the distroless image does not carry. Its
# own image, built as `--target checkouts`, so git never enters the gateway's.
FROM debian:bookworm-slim AS checkouts
RUN apt-get update && apt-get upgrade -y && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/* && useradd --uid 65532 --no-create-home --shell /usr/sbin/nologin nonroot
COPY --from=build /src/target/release/jcctl /usr/local/bin/jcctl
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/jcctl", "checkouts"]

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/context-gateway /usr/local/bin/context-gateway
COPY --from=build /src/target/release/jcctl /usr/local/bin/jcctl
COPY --from=build /src/target/release/jc-agent-proxy /usr/local/bin/jc-agent-proxy
COPY --from=build /src/target/release/jc-functions /usr/local/bin/jc-functions
# The knowledge assistant's crawl worker (Architecture/22, T-3052); its Deployment names it as the
# command, as the agent proxy's does.
COPY --from=build /src/target/release/jc-assistant /usr/local/bin/jc-assistant
COPY --from=build /opt/e5-small /opt/e5-small
COPY --from=build /opt/onnxruntime/libonnxruntime.so /usr/local/lib/libonnxruntime.so
ENV JC_ASSISTANT_MODEL_DIR=/opt/e5-small ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so
USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/context-gateway"]
