// Playwright driver for the WebUSB conformance harness: serves index.html + the
// wasm pkg, launches Chromium under Xvfb, clicks Run, asserts PASS.
//
// Env: CHROMIUM = path to the chromium/chrome binary to use.

import { chromium } from "playwright";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, normalize } from "node:path";

const DIR = dirname(fileURLToPath(import.meta.url));

const MIME = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".wasm": "application/wasm",
  ".json": "application/json",
  ".ts": "text/plain",
};

function startServer() {
  const server = createServer(async (req, res) => {
    try {
      let path = normalize(decodeURIComponent(req.url.split("?")[0]));
      if (path === "/favicon.ico") {
        res.writeHead(204).end();
        return;
      }
      if (path === "/" || path === "\\") path = "/index.html";
      const file = join(DIR, path);
      if (!file.startsWith(DIR)) {
        res.writeHead(403).end("forbidden");
        return;
      }
      const body = await readFile(file);
      const ext = path.slice(path.lastIndexOf("."));
      res.writeHead(200, { "content-type": MIME[ext] || "application/octet-stream" });
      res.end(body);
      console.log("[srv] 200", path);
    } catch {
      res.writeHead(404).end("not found");
      console.log("[srv] 404", req.url);
    }
  });
  // Fixed port so the WebUsbAllowDevicesForUrls policy origin matches.
  return new Promise((resolve) => server.listen(PORT, "127.0.0.1", () => resolve(server)));
}

const PORT = 8098;

async function main() {
  const server = await startServer();
  const url = `http://localhost:${PORT}/index.html`;

  const browser = await chromium.launch({
    executablePath: process.env.CHROMIUM || undefined,
    headless: false, // WebUSB needs full Chrome (run under Xvfb)
    args: ["--no-sandbox"],
  });
  const context = await browser.newContext();
  const page = await context.newPage();

  page.on("console", (m) => console.log("[page]", m.text()));
  page.on("pageerror", (e) => console.log("[pageerror]", e.message));
  page.on("requestfailed", (r) =>
    console.log("[reqfail]", r.url(), r.failure() && r.failure().errorText)
  );

  // The WebUsbAllowDevicesForUrls policy (see run.sh) pre-grants the fixture, so
  // device_list() sees it without a chooser.
  await page.goto(url);
  await page.click("#run");
  try {
    await page.waitForFunction(() => window.__hidraResult !== null, { timeout: 20000 });
  } catch (e) {
    const text = await page.evaluate(() => document.getElementById("result").textContent);
    console.error("[timeout] #result =", JSON.stringify(text));
    throw e;
  }
  const result = await page.evaluate(() => window.__hidraResult);

  await browser.close();
  server.close();

  if (result && result.ok) {
    console.log("WEBUSB_RESULT_OK", result.summary);
    process.exit(0);
  } else {
    console.error("WEBUSB_RESULT_FAIL", result && result.error);
    process.exit(1);
  }
}

main().catch((e) => {
  console.error("driver error:", e);
  process.exit(2);
});
