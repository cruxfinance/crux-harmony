# Stage 1: Build the Rust project
FROM rust:slim-bullseye as base

WORKDIR /app

RUN apt-get update && apt-get install -y pkg-config openssl libssl-dev

RUN cargo install cargo-chef

FROM base as planner

COPY . .

RUN cargo chef prepare --recipe-path recipe.json

# Stage 1: Build the Rust project
FROM base as builder

# Copy the top-level Cargo.toml and Cargo.lock files to optimize caching
COPY Cargo.toml Cargo.lock ./

# Copy the prepared recipe from the planner stage
COPY --from=planner /app/recipe.json ./recipe.json

# Build and cache dependencies for the workspace using the prepared recipe
RUN cargo chef cook --release --recipe-path recipe.json

# Copy the rest of the source code
COPY . .

# Build the individual packages within the workspace
RUN cargo build --release

# Stage 2: Create the final lightweight image
FROM debian:bullseye-slim

RUN apt-get update && apt-get install -y ca-certificates openssl libssl-dev

# Set the working directory
WORKDIR /app

# Copy the compiled binaries from the builder stage
COPY --from=builder /app/target/release/crux-harmony /app/

# Specify the default command to run when the container starts
CMD ["./crux-harmony"]

# Optionally, you can expose ports if the main_package binary listens on specific ports
EXPOSE 8100