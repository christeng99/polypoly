# Use official Rust image from Docker Hub as the base image
FROM rust:1.67 as builder

# Set the working directory inside the container
WORKDIR /usr/src/app

# Copy the Cargo.toml and Cargo.lock files to download dependencies early
COPY Cargo.toml Cargo.lock ./

# Create a dummy source file to fetch the dependencies first
RUN mkdir src && echo "fn main() {}" > src/main.rs

# Install the dependencies
RUN cargo build --release

# Now copy the entire application code
COPY . .

# Build the actual application
RUN cargo build --release

# Create a smaller final image
FROM debian:bullseye-slim

# Install necessary runtime dependencies (e.g., OpenSSL)
RUN apt-get update && apt-get install -y \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Set the working directory
WORKDIR /usr/local/bin

# Copy the compiled binary from the builder image
COPY --from=builder /usr/src/app/target/release/my_app /usr/local/bin/

# Set the entrypoint to the compiled binary
ENTRYPOINT ["/usr/local/bin/my_app"]

# Expose the port your app will run on (if necessary)
# EXPOSE 8080