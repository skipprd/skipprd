# Use the official Rust image as Base
#FROM rust:latest as builder

# Set the working directory in the Docker image
#WORKDIR /usr/src/skipprd

# Copy the Rust project to the image
#COPY . .

# Download the cross-compilation tools
#RUN rustup target add x86_64-unknown-linux-gnu

# @todo - this causes Fargate containers to fail :-(
# Install standard C library for x86_64
#ENV RUSTFLAGS="-C target-cpu=native"

# Build the Rust project
#RUN cargo build --release
#RUN cargo build --target x86_64-unknown-linux-gnu

# Set the start command to run your binary
#CMD ["./target/x86_64-unknown-linux-gnu/debug/skipprd"]
#CMD ["./target/release/skipprd"]

FROM ubuntu:latest
RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*
#COPY skipprd-linux_x86 /usr/bin/skipprd
RUN curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | bash

ENTRYPOINT ["skippr"]
CMD ["sync"]