const { chromium } = require('playwright');

(async () => {
  const browser = await chromium.launch({ headless: true });
  
  // 1. Desktop test
  const pageDesk = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  await pageDesk.goto('http://localhost:43100/', { waitUntil: 'domcontentloaded' });
  await pageDesk.click('.ws-tab[data-id="edumind"]');
  await pageDesk.waitForTimeout(500);
  await pageDesk.screenshot({ path: '/tmp/edumind_desktop_tab.png' });

  // 2. Mobile test
  const pageMob = await browser.newPage({ viewport: { width: 390, height: 844 } });
  await pageMob.goto('http://localhost:43100/', { waitUntil: 'domcontentloaded' });
  await pageMob.click('.ws-tab[data-id="edumind"]');
  await pageMob.waitForTimeout(500);
  await pageMob.screenshot({ path: '/tmp/edumind_mobile_tab.png' });

  await browser.close();
  console.log("Screenshots captured successfully!");
})();
