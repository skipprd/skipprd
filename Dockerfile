FROM rust:latest as builder
WORKDIR /usr/src/skipprd

COPY . .

#RUN rustup target add x86_64-unknown-linux-musl
#RUN apt update && apt install -y musl-tools musl-dev
#RUN update-ca-certificates
#
#RUN RUST_BACKTRACE=1 cargo build --target x86_64-unknown-linux-musl --release

FROM rust:latest
COPY --from=builder /usr/src/skipprd/target/x86_64-unknown-linux-musl/release/skipprd /usr/bin/skipprd
CMD ["skipprd", "sync"]