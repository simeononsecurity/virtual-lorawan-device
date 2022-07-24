# ------------------------------------------------------------------------------
# Cargo Build Stage
# ------------------------------------------------------------------------------

FROM rust:latest as cargo-build

RUN apt-get update && \
    apt-get upgrade -y && \
    apt-get install -y musl-tools
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /tmp/virtual-lorawan-device
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl

# ------------------------------------------------------------------------------
# Final Stage
# ------------------------------------------------------------------------------
FROM alpine:latest

LABEL org.opencontainers.image.source="https://github.com/simeononsecurity/virtual-lorawan-device"
LABEL org.opencontainers.image.description="A utility that attaches to a Semtech UDP Host and pretends to be a LoRaWAN Device"
LABEL org.opencontainers.image.authors="simeononsecurity"

COPY --from=cargo-build /tmp/virtual-lorawan-device/target/x86_64-unknown-linux-musl/release/virtual-lorawan-device /usr/local/bin/virtual-lorawan-device
RUN mkdir /etc/virtual-lorawan-device
COPY settings/default.toml /etc/virtual-lorawan-device/default.toml
CMD ["virtual-lorawan-device", "--settings", "/etc/virtual-lorawan-device"]
