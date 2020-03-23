#!/bin/sh

#may need to run once before:
# docker build -t litecore-rust-build

docker run -it --mount src="$(pwd)",target=/workspace,type=bind litecore-rust-build

# libLiteCore.so should be in target/debug/ YOU SHOULD CHANGE OWNER AND PERMISSIONS
