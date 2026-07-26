# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Pietrangelo Masala
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU Affero General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU Affero General Public License for more details.
#
# You should have received a copy of the GNU Affero General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.

# Local-testing image for system-agent. Tracks latest stable Rust per CLAUDE.md's
# toolchain policy — bump the base image, not a rust-toolchain.toml pin.
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    iproute2 \
    procps \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --home-dir /app agent
WORKDIR /app
COPY --from=builder /app/target/release/system-agent /usr/local/bin/system-agent
COPY static ./static
RUN chown -R agent:agent /app

USER agent
EXPOSE 9090
ENTRYPOINT ["system-agent"]
