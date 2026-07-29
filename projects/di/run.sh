#!/usr/bin/env bash
# AI 战略顾问 —— 一键启动。连上数据库,浏览器里选库、提问。
#   LLM_KEY=sk-... ./run.sh
# 可选:LLM_MODEL(默认 gemini-3.6-flash-high)、LLM_BASE、DI_SERVER_DSN、PORT
# 治理模式可选:DI_MODELS(默认 ./models)、DI_BIN(默认 PATH 里的 di)
set -euo pipefail
cd "$(dirname "$0")"          # 自成 workspace,就在本目录里构建
export LLM_KEY="${LLM_KEY:?请先设置 LLM_KEY(模型 API key)}"
export DI_SERVER_DSN="${DI_SERVER_DSN:-postgres://reformd:reformd@localhost:47615/conglomerate?sslmode=disable}"
export PORT="${PORT:-43200}"
cargo build -q
echo "→ 打开 http://localhost:$PORT"
exec ./target/debug/di-server
