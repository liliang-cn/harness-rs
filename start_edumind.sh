#!/usr/bin/env bash
export PORT=43300
export LLM_MODEL=gpt-5.6-terra
export LLM_BASE=https://cpa.superleo.app/v1
export LLM_KEY=sk-cpa-211f4cbd146aa63f69730022ecca6420
export CORTEXDB_MCP_BIN=/Users/liliang/.codex/plugins/cache/cortexdb/cortexdb/2.57.0/bin/cortexdb-mcp

exec ./target/debug/edumind-tutor-agent
