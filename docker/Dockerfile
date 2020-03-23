FROM debian:buster

RUN apt-get update && apt-get upgrade --assume-yes && apt-get  --assume-yes install curl ca-certificates \
    build-essential zlib1g-dev libssl-dev libicu-dev libicu63 pkg-config \
    clang llvm cmake bash

ENV PATH /root/.cargo/bin:$PATH
RUN curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain nightly -y

WORKDIR /workspace
CMD cargo build ; chmod -R a+rw target
