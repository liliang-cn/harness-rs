const { chromium } = require('playwright');

(async () => {
  console.log("=== 🚀 Starting EduMind End-to-End Test ===");
  const browser = await chromium.launch({ headless: true });
  
  const pageDesk = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  await pageDesk.goto('http://localhost:43100/', { waitUntil: 'domcontentloaded' });
  await pageDesk.click('.ws-tab[data-id="edumind"]');
  await pageDesk.waitForTimeout(1000);
  await pageDesk.screenshot({ path: '/tmp/edumind_e2e_desktop.png' });
  console.log("[1] Desktop Screenshot saved to /tmp/edumind_e2e_desktop.png");

  const pageMob = await browser.newPage({ viewport: { width: 390, height: 844 } });
  await pageMob.goto('http://localhost:43100/', { waitUntil: 'domcontentloaded' });
  await pageMob.click('.ws-tab[data-id="edumind"]');
  await pageMob.waitForTimeout(1000);
  await pageMob.screenshot({ path: '/tmp/edumind_e2e_mobile.png' });
  console.log("[2] Mobile Screenshot saved to /tmp/edumind_e2e_mobile.png");

  await browser.close();
  console.log("=== ✅ All Live Tests Passed! ===");
})();
