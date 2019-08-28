#!/usr/bin/env bash

set -eux

# Install rustup and the specified rust toolchain.
curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain=$1 -y

# Load cargo environment. Specifically, put cargo into PATH.
source ~/.cargo/env

rustc --version
rustup --version
cargo --version

case $2 in
	"native")
		sudo apt-get -y update
		sudo apt-get install -y cmake pkg-config libssl-dev

		cargo test --all --locked
		;;

	"wasm")
		# Install prerequisites and build all wasm projects
		./init.sh
		./build.sh --locked
		;;
esac