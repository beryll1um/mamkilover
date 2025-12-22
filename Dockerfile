# Copyright (c) 2025 Mamkilover's
# Distributed under the MIT software license, see the accompanying
# file COPYING or http://www.opensource.org/licenses/mit-license.php.

FROM rust:1.92-bookworm AS builder

# Install the libsqlite3 development package if it is not already installed.
RUN apt-get update && apt-get install -y libsqlite3-dev && rm -rf /var/lib/apt/lists/*

# I haven't found a better way to pre-build dependencies.
RUN mkdir src && echo 'fn main() { println!("Dummy!"); }' > src/main.rs
COPY Cargo.toml Cargo.lock .
RUN cargo build --release

# Copy the source code and manually change the modification time of the main file
# to force the Rust compiler to use the new version.
COPY src/ src/
RUN touch -m src/main.rs
RUN cargo build --release

FROM gcr.io/distroless/cc-debian12
# Currently, it's not possible to statically link this dependency.
COPY --from=builder /usr/lib/x86_64-linux-gnu/libsqlite3.so* /usr/lib/x86_64-linux-gnu/
COPY --from=builder /target/release/mamkilover /usr/local/bin/
ENTRYPOINT ["mamkilover"]

