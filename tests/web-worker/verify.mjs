import { chromium } from "playwright-core";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const fixture = fileURLToPath(new URL("./", import.meta.url));
const build = process.env.WINIT_WORKER_PKG;
assert.ok(build, "WINIT_WORKER_PKG must point to the compiled fixture pkg directory");
assert.ok(process.env.CHROMIUM_BIN, "CHROMIUM_BIN must point to a Chromium executable");
const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_BIN,
  headless: true,
  args: ["--no-sandbox"],
});
try {
  const context = await browser.newContext();
  await context.route("https://winit.test/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    const file = path.startsWith("/pkg/")
      ? build + path.slice(4)
      : fixture + (path === "/" ? "index.html" : path.slice(1));
    await route.fulfill({
      body: await readFile(file),
      contentType: path.endsWith(".wasm") ? "application/wasm"
        : path.endsWith(".js") ? "text/javascript" : "text/html",
      headers: {
        "Cross-Origin-Opener-Policy": "same-origin",
        "Cross-Origin-Embedder-Policy": "require-corp",
      },
    });
  });
  const errors = [];
  const page = await context.newPage();
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  page.on("pageerror", (error) => errors.push(String(error)));
  await page.goto("https://winit.test/");
  await page.waitForFunction(() => window.ready, null, { timeout: 30000 });
  assert.equal(await page.evaluate(() => window.ready), "ready");
  await page.waitForFunction(() => wasm.stats()[0] > 5);

  const progress = await page.evaluate(() => {
    const before = wasm.stats()[0];
    const end = performance.now() + 1000;
    while (performance.now() < end) {}
    return wasm.stats()[0] - before;
  });
  assert.ok(progress > 15, `only ${progress} frames while page blocked`);
  console.log("frames during one-second page block:", progress);

  const canvas = page.locator("canvas");
  assert.equal(await canvas.getAttribute("alt"), "worker fixture");
  assert.equal(await canvas.getAttribute("tabindex"), null);
  assert.equal(await canvas.evaluate((el) => el.style.minWidth), "100px");
  assert.equal(await canvas.evaluate((el) => el.style.maxWidth), "800px");
  await page.evaluate(() => wasm.wake(4));
  await new Promise((resolve) => setTimeout(resolve, 100));
  const uncapped = await page.evaluate(() => {
    const before = wasm.stats()[0];
    const end = performance.now() + 1000;
    while (performance.now() < end) {}
    return wasm.stats()[0] - before;
  });
  assert.ok(uncapped > progress * 1.5, `${uncapped} immediate frames vs ${progress} rAF frames`);
  console.log("uncapped frames during one-second page block:", uncapped);
  await page.evaluate(() => wasm.wake(5));
  await new Promise((resolve) => setTimeout(resolve, 100));
  const restored = await page.evaluate(() => {
    const before = wasm.stats()[0];
    const end = performance.now() + 1000;
    while (performance.now() < end) {}
    return wasm.stats()[0] - before;
  });
  assert.ok(restored > 15 && restored < progress * 1.5, `rAF not restored: ${restored}`);
  await page.evaluate(() => wasm.wake(6));
  await page.waitForFunction(() => document.querySelector("canvas").getAttribute("alt") === "updated on worker");
  assert.equal(await canvas.evaluate((el) => el.style.minWidth), "120px");
  assert.equal(await canvas.evaluate((el) => el.style.maxWidth), "750px");
  assert.equal(await canvas.evaluate((el) => getComputedStyle(el).cursor), "crosshair");
  await page.evaluate(() => wasm.wake(7));
  await page.waitForFunction(() => getComputedStyle(document.querySelector("canvas")).cursor === "none");
  await page.evaluate(() => wasm.wake(8));
  await page.waitForFunction(() => getComputedStyle(document.querySelector("canvas")).cursor === "crosshair");
  await canvas.evaluate((el) => { el.tabIndex = 0; });
  await page.locator("canvas").click();
  await page.keyboard.press("w");
  await page.evaluate(() => wasm.wake(10));
  await page.waitForFunction(() => document.pointerLockElement === document.querySelector("canvas"));
  await page.evaluate(() => wasm.wake(8));
  await page.waitForFunction(() => !document.pointerLockElement);
  await canvas.click();
  await page.evaluate(() => wasm.wake(9));
  await page.waitForFunction(() => document.fullscreenElement === document.querySelector("canvas"));
  await page.evaluate(() => wasm.wake(8));
  await page.waitForFunction(() => !document.fullscreenElement);
  await page.waitForFunction(() => wasm.stats()[1] > 0);
  await page.locator("canvas").evaluate((element) => { element.style.width = "500px"; });
  await page.waitForFunction(() => wasm.stats()[2] === 500);

  const cdp = await context.newCDPSession(page);
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 800, height: 600, deviceScaleFactor: 2, mobile: false,
  });
  await page.waitForFunction(() => document.querySelector("canvas").style.width === "210px");

  await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent("pagehide", { persisted: true })));
  await page.waitForFunction(() => wasm.stats()[7] === 1);
  await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent("pageshow", { persisted: true })));
  await page.waitForFunction(() => wasm.stats()[6] === 2);
  const wakes = await page.evaluate(() => { const count = wasm.stats()[3]; wasm.wake(0); return count; });
  await page.waitForFunction((count) => wasm.stats()[3] > count, wakes);
  const before = await page.evaluate(() => wasm.stats()[0]);
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(await page.evaluate(() => wasm.stats()[0]), before);
  await page.evaluate(() => wasm.wake(3));
  await page.waitForFunction(() => wasm.stats()[5] === 1);
  await page.evaluate(() => wasm.wake(1));
  await page.waitForFunction((count) => wasm.stats()[0] > count, before);
  await page.evaluate(() => wasm.wake(2));
  await page.waitForFunction(() => wasm.stats()[4] === 1);
  await new Promise((resolve) => setTimeout(resolve, 200));
  assert.deepEqual(errors, []);
  console.log("PASS: redraw modes, window attributes, cursor, pointer lock, fullscreen, lifecycle forwarding, input, DPI, Wait, WaitUntil, proxy wake, and exit");
} finally {
  await browser.close();
}
