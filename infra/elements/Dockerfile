FROM debian:bookworm-slim

ARG ELEMENTS_VERSION=23.3.4
ARG ELEMENTS_ARCH=aarch64-linux-gnu
ARG ELEMENTS_SHA256=ee20cc20302bf7a41a063241f59f17a0a2e4d18479c2fff9cbdbfc6c25919510

RUN apt-get update \
  && apt-get install --no-install-recommends --yes ca-certificates curl \
  && rm -rf /var/lib/apt/lists/* \
  && curl --fail --silent --show-error --location \
    "https://github.com/ElementsProject/elements/releases/download/elements-${ELEMENTS_VERSION}/elements-${ELEMENTS_VERSION}-${ELEMENTS_ARCH}.tar.gz" \
    --output /tmp/elements.tar.gz \
  && echo "${ELEMENTS_SHA256}  /tmp/elements.tar.gz" | sha256sum --check --strict \
  && tar --extract --gzip --file /tmp/elements.tar.gz --directory /usr/local/bin --strip-components=2 --wildcards "*/bin/elementsd" "*/bin/elements-cli" \
  && rm /tmp/elements.tar.gz

ENTRYPOINT ["/usr/local/bin/elementsd"]
