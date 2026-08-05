import subprocess
import time
import os

env = os.environ.copy()
env["PORT"] = "43300"
env["LLM_MODEL"] = "gpt-5.6-terra"
env["LLM_BASE"] = "https://cpa.superleo.app/v1"
env["LLM_KEY"] = "sk-cpa-211f4cbd146aa63f69730022ecca6420"
env["CORTEXDB_MCP_BIN"] = "/Users/liliang/.codex/plugins/cache/cortexdb/cortexdb/2.57.0/bin/cortexdb-mcp"

proc = subprocess.Popen(
    ["./target/debug/edumind-tutor-agent"],
    env=env,
    stdout=open("/tmp/edumind_srv.log", "w"),
    stderr=subprocess.STDOUT,
    start_new_session=True
)
print("EduMind process launched with PID:", proc.pid)
