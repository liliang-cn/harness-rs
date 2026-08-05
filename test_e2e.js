const { chromium } = require('playwright');

(async () => {
  console.log("=== 🚀 Running E2E Test for EduMind AI ===");
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  
  await page.goto('http://localhost:43100/');
  await page.waitForTimeout(1000);

  // 点击 EduMind 智学家教
  console.log("1. Clicking EduMind workspace tab...");
  await page.click('.ws-tab[data-id="edumind"]');
  await page.waitForTimeout(1000);

  // 快捷发送 preset hint "帮我用配方法解方程 x^2 - 6x + 5 = 0"
  console.log("2. Clicking preset question hint...");
  await page.click('.hint');
  await page.waitForTimeout(4000);

  // 截图聊天记录
  await page.screenshot({ path: '/tmp/edumind_chat_result.png' });
  console.log("3. Screenshot saved to /tmp/edumind_chat_result.png");

  // 390x844 Mobile Viewport
  const pageMobile = await browser.newPage({ viewport: { width: 390, height: 844 } });
  await pageMobile.goto('http://localhost:43100/');
  await pageMobile.waitForTimeout(1000);
  await pageMobile.click('.ws-tab[data-id="edumind"]');
  await pageMobile.screenshot({ path: '/tmp/edumind_mobile_final.png' });
  console.log("4. Mobile Screenshot saved to /tmp/edumind_mobile_final.png");

  await browser.close();
  console.log("=== ✅ E2E Testing Finished ===");
})();
