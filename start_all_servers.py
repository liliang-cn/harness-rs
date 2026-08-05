import subprocess
import os
import time

env = os.environ.copy()
env["LLM_MODEL"] = "gpt-5.6-terra"
env["LLM_BASE"] = "https://cpa.superleo.app/v1"
env["LLM_KEY"] = "sk-cpa-211f4cbd146aa63f69730022ecca6420"
env["CORTEXDB_MCP_BIN"] = "/Users/liliang/.codex/plugins/cache/cortexdb/cortexdb/2.57.0/bin/cortexdb-mcp"

# 1. 启动 EduMind Tutor Agent (43300)
p1 = subprocess.Popen(
    ["./target/debug/edumind-tutor-agent"],
    env=env,
    stdout=open("/tmp/edumind_srv.log", "w"),
    stderr=subprocess.STDOUT,
    start_new_session=True
)

# 2. 启动 Web 托管 (43100)
p2 = subprocess.Popen(
    ["python3", "-m", "http.server", "43100", "--directory", "projects/datainside/bi-server/web"],
    stdout=open("/tmp/web_srv.log", "w"),
    stderr=subprocess.STDOUT,
    start_new_session=True
)

print(f"EduMind Agent started PID {p1.pid}, Web Server started PID {p2.pid}")
