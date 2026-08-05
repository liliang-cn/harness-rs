const { chromium } = require('playwright');

(async () => {
  console.log("=== 🚀 Starting MCP Playwright Testing for EduMind AI ===");
  const browser = await chromium.launch({ headless: true });
  
  // 1. 测试 Desktop Viewport (1280x800)
  const contextDesk = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const pageDesk = await contextDesk.newPage();
  await pageDesk.goto('http://localhost:43100/');
  await pageDesk.waitForTimeout(1000);

  // 点击 EduMind 智学家教 Workspace
  console.log("[Desktop] Selecting EduMind Tutor workspace...");
  await pageDesk.click('.ws-tab[data-id="edumind"]');
  await pageDesk.waitForTimeout(1000);

  // 截屏 desktop 状态
  await pageDesk.screenshot({ path: '/tmp/edumind_desktop.png' });
  console.log("[Desktop] Screenshot saved to /tmp/edumind_desktop.png");

  // 2. 测试 Mobile Viewport (iPhone 14 / 390x844)
  const contextMobile = await browser.newContext({
    viewport: { width: 390, height: 844 },
    userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X) AppleWebKit/605.1.15'
  });
  const pageMobile = await contextMobile.newPage();
  await pageMobile.goto('http://localhost:43100/');
  await pageMobile.waitForTimeout(1000);
  await pageMobile.click('.ws-tab[data-id="edumind"]');
  await pageMobile.waitForTimeout(1000);

  // 截屏 mobile 状态
  await pageMobile.screenshot({ path: '/tmp/edumind_mobile.png' });
  console.log("[Mobile] Screenshot saved to /tmp/edumind_mobile.png");

  await browser.close();
  console.log("=== ✅ Playwright Testing Completed ===");
})();
