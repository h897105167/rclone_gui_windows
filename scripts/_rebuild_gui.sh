#!/usr/bin/env bash
# 测试 rc crate 的错误分类，再重编译 GUI（release）。
set -u
cd "$(dirname "$0")/.." || exit 1
echo "== cargo test -p rcgui-rc =="
cargo test -p rcgui-rc 2>&1 | tail -25
echo "test exit: ${PIPESTATUS[0]}"
echo "== cargo build --release =="
cargo build --release 2>&1 | tail -15
echo "build exit: ${PIPESTATUS[0]}"
ls -l target/release/rclone-gui.exe
