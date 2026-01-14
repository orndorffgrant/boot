#!/usr/bin/bash

set -eux -o pipefail

host=$1

cross build --target aarch64-unknown-linux-gnu
ssh $host sudo boot stop-server
scp target/aarch64-unknown-linux-gnu/debug/boot $host:/home/grant/boot-cli
ssh $host sudo boot start-server --host 0.0.0.0
