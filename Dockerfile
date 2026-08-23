# syntax=docker/dockerfile:1

ARG PHP_BASE=php:8.5-cli-trixie@sha256:54d82ff9be6bd198145e90c917fc9b2e24230b42e52def8deb3554baf61c451a
ARG RUST_BASE=rust:1-trixie@sha256:b1b3c9c0d921d7fa0a6d1f9ec7e4eab87f8c8ec97644c3d791450f131dec813f

FROM ${RUST_BASE} AS rust

FROM ${PHP_BASE} AS builder
ARG PHP_BASE

COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo /usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends clang libclang-dev; \
    rm -rf /var/lib/apt/lists/*

RUN rustup toolchain install stable --profile minimal --component rustfmt --component clippy

WORKDIR /src
COPY . .

ENV RUSTFLAGS="-C link-arg=-Wl,-rpath,/usr/local/lib"

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=rapira-target-${PHP_BASE},sharing=locked \
    set -eux; \
    cargo build --release --locked --bin rapira; \
    cp target/release/rapira /usr/local/bin/rapira; \
    ldd /usr/local/bin/rapira | grep -q '/usr/local/lib/libphp.so'

FROM ${PHP_BASE} AS payload

COPY --from=builder /usr/local/bin/rapira /out/usr/local/bin/rapira
COPY --from=builder /usr/local/lib/libphp.so /out/usr/local/lib/libphp.so

# PHP 8.4 builds opcache as a shared module and 8.5 links it into libphp, so the copy is conditional.
RUN set -eux; \
    ext_dir="$(php-config --extension-dir)"; \
    if [ -f "$ext_dir/opcache.so" ]; then \
        install -D "$ext_dir/opcache.so" "/out$ext_dir/opcache.so"; \
        install -D -m 0644 "$PHP_INI_DIR/conf.d/docker-php-ext-opcache.ini" \
                           "/out$PHP_INI_DIR/conf.d/docker-php-ext-opcache.ini"; \
    fi; \
    php --ri "Zend OPcache" > /dev/null

WORKDIR /out/usr/local/share/rapira

# Records the Debian packages owning shared objects the payload needs from outside /usr/local: https://manpages.debian.org/trixie/dpkg/dpkg-query.1.en.html#S
RUN find /out/usr/local -type f \( -executable -o -name '*.so' \) -exec ldd '{}' ';' 2>/dev/null \
        | awk '/=>/ { so = $(NF-1); if (index(so, "/usr/local/") == 1) { next }; gsub("^/(usr/)?", "", so); printf "*%s\n", so }' \
        | sort -u \
        | xargs -r dpkg-query --search \
        | awk 'sub(":$", "", $1) { print $1 }' \
        | sort -u > debian-packages.txt

RUN php -r 'echo PHP_VERSION, "\n";' > PHP_VERSION.txt

# The staged pair links and runs.
RUN /out/usr/local/bin/rapira --version

FROM scratch AS runtime

LABEL org.opencontainers.image.title="Rapira" \
      org.opencontainers.image.description="rapira binary and libphp.so, staged for COPY --from into your own image" \
      org.opencontainers.image.url="https://rapira.rs" \
      org.opencontainers.image.source="https://github.com/rapira-rs/rapira" \
      org.opencontainers.image.licenses="MIT"

COPY --from=payload /out/ /
