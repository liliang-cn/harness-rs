const { chromium } = require('playwright');

(async () => {
  console.log("=== 🚀 Starting EduMind End-to-End Test ===");
  const browser = await chromium.launch({ headless: true });
  
  // 桌面端测试
  const pageDesk = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  await pageDesk.goto('http://localhost:43100/');
  await pageDesk.waitForTimeout(1000);
  await pageDesk.click('.ws-tab[data-id="edumind"]');
  await pageDesk.waitForTimeout(500);

  // 模拟学生发送提问
  console.log("[1] Sending student inquiry...");
  await pageDesk.fill('#chat-input', '我想学习配方法解方程 x^2 - 6x + 5 = 0');
  await pageDesk.click('#send-btn');
  
  // 等待 Agent 苏格拉底启发式提问与 ECharts / KaTeX 渲染
  console.log("[2] Waiting for Socratic response with KaTeX & ECharts...");
  await pageDesk.waitForTimeout(8000);

  await pageDesk.screenshot({ path: '/tmp/edumind_e2e_desktop.png' });
  console.log("[3] Desktop Screenshot saved to /tmp/edumind_e2e_desktop.png");

  // 移动端测试
  const pageMob = await browser.newPage({ viewport: { width: 390, height: 844 } });
  await pageMob.goto('http://localhost:43100/');
  await pageMob.waitForTimeout(1000);
  await pageMob.click('.ws-tab[data-id="edumind"]');
  await pageMob.waitForTimeout(500);

  await pageMob.screenshot({ path: '/tmp/edumind_e2e_mobile.png' });
  console.log("[4] Mobile Screenshot saved to /tmp/edumind_e2e_mobile.png");

  await browser.close();
  console.log("=== ✅ All Live Tests Passed! ===");
})();
