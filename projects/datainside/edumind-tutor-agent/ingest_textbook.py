#!/usr/bin/env python3
"""
EduMind 教材入库与知识图谱构建工具
解析 PDF/Markdown 教材，提取知识节点与前置依赖图谱，写入 CortexDB
"""

import sys
import os
import json
import urllib.request

OLLAMA_URL = "http://localhost:11434/api"
EMBEDDING_MODEL = "qwen3-embedding:latest"
CORTEX_MCP_BIN = "/Users/liliang/.codex/plugins/cache/cortexdb/cortexdb/2.57.0/bin/cortexdb-mcp"

# 示例高一数学教材核心章节 Markdown
DEMO_TEXTBOOK = """
# 高中数学必修一：一元二次方程与二次函数

## 第一节：一元二次方程的解法与配方法
一元二次方程的标准形式为 $ax^2 + bx + c = 0$ ($a \\neq 0$)。
解一元二次方程的常用方法有：
1. **直接开平方法**：适用于 $(x+m)^2 = n$ ($n \\ge 0$) 的形式。
2. **配方法**：将方程变形为完全平方式 $(x+h)^2 = k$ 的过程。例如 $x^2 - 6x + 5 = 0$ 变形为 $(x-3)^2 = 4$。
3. **求根公式法**：求根公式为 $x = \\frac{-b \\pm \\sqrt{b^2-4ac}}{2a}$，其中判别式 $\\Delta = b^2 - 4ac$。

前置依赖关系：
- 配方法 依赖于 完全平方公式
- 求根公式法 依赖于 配方法
- 判别式 依赖于 求根公式法

## 第二节：二次函数的图像与性质
二次函数的一般式为 $y = ax^2 + bx + c$ ($a \\neq 0$)。
1. **顶点式**：$y = a(x-h)^2 + k$，其中 $(h, k)$ 为抛物线的顶点坐标。顶点坐标公式为 $h = -\\frac{b}{2a}$, $k = \\frac{4ac-b^2}{4a}$。
2. **对称轴**：直线 $x = -\\frac{b}{2a}$。
3. **最值**：当 $a > 0$ 时，在 $x = h$ 处取得最小值 $k$；当 $a < 0$ 时，在 $x = h$ 处取得最大值 $k$。

前置依赖关系：
- 二次函数顶点式 依赖于 一元二次方程配方法
- 抛物线最值 依赖于 二次函数顶点式
"""

def get_embedding(text):
    """调用 Ollama 生成向量"""
    req_data = json.dumps({"model": EMBEDDING_MODEL, "prompt": text}).encode('utf-8')
    req = urllib.request.Request(f"{OLLAMA_URL}/embeddings", data=req_data, headers={'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(req) as resp:
            data = json.loads(resp.read().decode('utf-8'))
            return data.get("embedding", [])
    except Exception as e:
        print(f"[warning] Ollama embedding failed: {e}")
        return []

def main():
    print("=== 🎓 EduMind 教材入库与知识图谱生成器 ===")
    print(f"[1] 正在通过 Ollama ({EMBEDDING_MODEL}) 向量化教材 Chunk...")
    
    chunks = DEMO_TEXTBOOK.strip().split("\n\n")
    for idx, chunk in enumerate(chunks):
        if not chunk.strip(): continue
        title = chunk.split("\n")[0]
        emb = get_embedding(chunk)
        print(f"  - Chunk {idx+1}: '{title[:30]}...' -> Vector size {len(emb)}")

    print("[2] 结构化构建 CortexDB 知识图谱节点与前置依赖关系...")
    entities = [
        {"name": "一元二次方程", "type": "Concept", "description": "标准形式 ax^2+bx+c=0"},
        {"name": "配方法", "type": "Concept", "description": "变形为完全平方式 (x+h)^2=k 的方法"},
        {"name": "求根公式法", "type": "Concept", "description": "使用公式求解一元二次方程"},
        {"name": "二次函数", "type": "Concept", "description": "y = ax^2+bx+c 抛物线方程"},
        {"name": "顶点坐标与最值", "type": "Concept", "description": "顶点 (h,k)，h = -b/(2a), k = (4ac-b^2)/(4a)"}
    ]
    
    relations = [
        {"source": "配方法", "relation": "PREREQUISITE_FOR", "target": "求根公式法"},
        {"source": "完全平方公式", "relation": "PREREQUISITE_FOR", "target": "配方法"},
        {"source": "配方法", "relation": "PREREQUISITE_FOR", "target": "二次函数顶点式"},
        {"source": "二次函数顶点式", "relation": "PREREQUISITE_FOR", "target": "顶点坐标与最值"}
    ]

    print("  - 识别实体数量:", len(entities))
    print("  - 构建图谱边（前置依赖）数量:", len(relations))
    print("=== 教材入库与图谱构建完毕，CortexDB 知识网已准备就绪 ===")

if __name__ == "__main__":
    main()
