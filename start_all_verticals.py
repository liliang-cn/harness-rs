import subprocess
import os
import time

env = os.environ.copy()
env["LLM_MODEL"] = "gpt-5.6-terra"
env["LLM_BASE"] = "https://cpa.superleo.app/v1"
env["LLM_KEY"] = "sk-cpa-211f4cbd146aa63f69730022ecca6420"
env["DI_BIN"] = "/Users/liliang/Things/AI/base/dataintelligence/di"
env["DSN_BASE"] = "postgres://reformd:reformd@localhost:47615"
env["CORTEXDB_MCP_BIN"] = "/Users/liliang/.codex/plugins/cache/cortexdb/cortexdb/2.57.0/bin/cortexdb-mcp"

# 1. 启动 EduMind Tutor Agent (43300)
p1 = subprocess.Popen(
    ["./target/debug/edumind-tutor-agent"],
    env=env,
    stdout=open("/tmp/edumind_srv.log", "w"),
    stderr=subprocess.STDOUT,
    start_new_session=True
)

# 2. 启动 13 个 BI Server 实例 (43117 - 43129)
ROWS = [
  ("retail-bi-agent", "projects/datainside/retail-bi-agent/di/model.yaml", "applehub", "43117", "finance"),
  ("supermarket-bi-agent", "projects/datainside/supermarket-bi-agent/di/model.yaml", "supermart", "43118", "finance"),
  ("teahouse-bi-agent", "projects/datainside/teahouse-bi-agent/di/model.yaml", "teahouse", "43119", "finance"),
  ("forge-ops-agent", "projects/datainside/forge-ops-agent/di/model.yaml", "forge", "43120", "finance"),
  ("hotel-revenue-agent", "projects/datainside/hotel-revenue-agent/di/model.yaml", "hotel", "43121", "finance"),
  ("pharmacy-bi-agent", "projects/datainside/pharmacy-bi-agent/di/model.yaml", "pharmacy", "43122", "finance"),
  ("autorepair-ops-agent", "projects/datainside/autorepair-ops-agent/di/model.yaml", "autorepair", "43123", "finance"),
  ("dental-clinic-agent", "projects/datainside/dental-clinic-agent/di/model.yaml", "dental", "43124", "finance"),
  ("gym-membership-agent", "/Users/liliang/Things/AI/base/dataintelligence/examples/fitness/model.yaml", "reformd", "43125", "analyst"),
  ("warehouse-bi-agent", "projects/datainside/warehouse-bi-agent/di/model.yaml", "warehouse", "43126", "finance"),
  ("manufacturing-quality-agent", "projects/datainside/manufacturing-quality-agent/di/model.yaml", "manufacturing", "43127", "quality"),
  ("supermart-ingest", "projects/datainside/supermarket-bi-agent/di/model.yaml", "supermart_ingest", "43128", "finance"),
  ("ecommerce-bi-agent", "projects/datainside/ecommerce-bi-agent/di/model.yaml", "ecommerce", "43129", "finance")
]

bi_pids = []
for id_name, model_path, db, port, role in ROWS:
    benv = env.copy()
    benv["DI_MODEL"] = os.path.abspath(model_path)
    benv["DI_DSN"] = f"postgres://reformd:reformd@localhost:47615/{db}?sslmode=disable"
    benv["PORT"] = port
    benv["DI_ROLE"] = role
    benv["AUDIT"] = f"/tmp/bi-server-{id_name}-audit.jsonl"
    p = subprocess.Popen(
        ["./target/debug/bi-server"],
        env=benv,
        stdout=open(f"/tmp/bi-server-{id_name}.log", "w"),
        stderr=subprocess.STDOUT,
        start_new_session=True
    )
    bi_pids.append(p.pid)

# 3. 启动 Web Server (43100)
p3 = subprocess.Popen(
    ["python3", "-m", "http.server", "43100", "--directory", "projects/datainside/bi-server/web"],
    stdout=open("/tmp/web_srv.log", "w"),
    stderr=subprocess.STDOUT,
    start_new_session=True
)

print(f"✅ Launched EduMind (43300), 13 BI Servers (43117-43129), and Web (43100). Web PID: {p3.pid}")
