with open("projects/datainside/bi-server/web/index.html", "r", encoding="utf-8") as f:
    html = f.read()

# 自动将 EduMind 智学家教设置为初始默认选中的工作区
html = html.replace('let currentWS  = null', 'let currentWS  = WS[0]')
html = html.replace('AI 经营助手', 'EduMind AI 智学助手')
html = html.replace('v === "dashboard" ? "数据看板" : "数据源"', 'v === "dashboard" ? "学习数据看板" : "知识库与图谱"')

with open("projects/datainside/bi-server/web/index.html", "w", encoding="utf-8") as f:
    f.write(html)

print("Updated index.html: Default workspace set to EduMind 智学家教")
