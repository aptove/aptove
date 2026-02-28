FROM debian:bookworm-slim

ARG TARGETARCH

LABEL org.opencontainers.image.source=https://github.com/aptove/aptove
LABEL org.opencontainers.image.description="Aptove ACP AI coding agent"
LABEL org.opencontainers.image.licenses=Apache-2.0

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY bin/aptove /usr/local/bin/aptove
RUN chmod +x /usr/local/bin/aptove

ENTRYPOINT ["/usr/local/bin/aptove"]
CMD ["run"]
