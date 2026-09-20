# Model Tools: the one non-Rust runtime component (Architecture/11 §6.5, DM-18).
# Published by .github/workflows/image.yml as ghcr.io/marek-mraz/joinedcontext-platform/model-tools
# and pinned by digest in the deployment; DM-19 pins the same version for CI and the preview.
#
# Build context is this folder: `docker build tools/model-tools`.
FROM python:3.11-slim-bookworm AS build
WORKDIR /src
# Every dependency is pinned exactly in pyproject.toml, so the image and `pip install .[test]`
# in CI resolve to the same generator (DM-19). No index other than the default is configured.
COPY pyproject.toml ./
COPY src ./src
COPY models ./models
RUN python -m venv /opt/venv \
 && /opt/venv/bin/pip install --no-cache-dir --upgrade pip setuptools \
 && /opt/venv/bin/pip install --no-cache-dir . \
 # The installer is not part of the runtime: Model Tools installs nothing, and pip and
 # setuptools carry a vendored tree (`setuptools/_vendor/wheel`, `jaraco.context`) that is
 # reported against the image for a package the image never runs. Removed, not ignored.
 && rm -rf /opt/venv/lib/python3.11/site-packages/pip \
           /opt/venv/lib/python3.11/site-packages/pip-*.dist-info \
           /opt/venv/lib/python3.11/site-packages/setuptools \
           /opt/venv/lib/python3.11/site-packages/setuptools-*.dist-info \
           /opt/venv/lib/python3.11/site-packages/pkg_resources \
           /opt/venv/lib/python3.11/site-packages/wheel \
           /opt/venv/lib/python3.11/site-packages/wheel-*.dist-info \
           /opt/venv/bin/pip /opt/venv/bin/pip3 /opt/venv/bin/pip3.11

FROM python:3.11-slim-bookworm
# The interpreter image lags Debian's security updates by days (pcre2, T-0574): the runtime
# stage takes them itself, so the scan judges what is shipped rather than what was published.
RUN apt-get update \
    && apt-get upgrade -y --no-install-recommends \
    && rm -rf /var/lib/apt/lists/*
# Nothing in this image is written to at runtime: Model Tools reads a LinkML document and
# answers with artifacts, and the only state it holds is the catalogue cache in memory (DM-12).
COPY --from=build /opt/venv /opt/venv
COPY --from=build /src/models /opt/model-tools/models
# The interpreter image ships its own installer too, and nothing in this container installs
# anything: the venv is complete and read-only in practice.
RUN rm -rf /usr/local/lib/python3.11/site-packages/pip \
           /usr/local/lib/python3.11/site-packages/pip-*.dist-info \
           /usr/local/lib/python3.11/site-packages/setuptools \
           /usr/local/lib/python3.11/site-packages/setuptools-*.dist-info \
           /usr/local/lib/python3.11/site-packages/pkg_resources \
           /usr/local/lib/python3.11/site-packages/wheel \
           /usr/local/lib/python3.11/site-packages/wheel-*.dist-info \
           /usr/local/bin/pip /usr/local/bin/pip3 /usr/local/bin/pip3.11
ENV PATH=/opt/venv/bin:$PATH \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    MODEL_TOOLS_PORT=8080 \
    MODEL_TOOLS_MODELS=/opt/model-tools/models
RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin model-tools
USER 10001:10001
EXPOSE 8080
# `python -m service` is the HTTP face; the generators stay runnable as
# `python -m gen_json_schema …` for a one-off render inside the same image.
ENTRYPOINT ["python", "-m", "service"]
