import { chromium, test, expect } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import { cleanupBrowserProcesses, clearAppData, forceKill, getFreePort, waitForCdp } from "./e2e-helpers";

const __dirname = fileURLToPath(new URL(".", import.meta.url));
const APP_PATH = resolve(__dirname, "../../target/release/babel-ebook-desktop.exe");

import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";

let appProcess: ChildProcess | null = null;
let cdpUrl: string;
let checkpointDir: string;
let sourcePath: string;

test.beforeAll(async () => {
  await cleanupBrowserProcesses();
  clearAppData();

  checkpointDir = mkdtempSync(resolve(tmpdir(), "babel-checkpoint-e2e-"));
  sourcePath = resolve(checkpointDir, "Dead Mountain checkpoint fixture.epub");
  const sourceContent = "checkpoint source fixture";
  writeFileSync(sourcePath, sourceContent);
  const sourceHash = createHash("sha256").update(sourceContent).digest("hex");
  writeFileSync(resolve(checkpointDir, "checkpoint-ui-test.json"), JSON.stringify({
    job_id: "checkpoint-ui-test",
    source_hash: sourceHash,
    translation_signature: "fixture-signature",
    source_path: sourcePath,
    chapters: [
      { index: 0, href: "part0034.xhtml", status: "completed", content: [], error: null },
      { index: 1, href: "part0035.xhtml", status: "failed", content: null, error: "fixture failure" },
      { index: 2, href: "part0036.xhtml", status: "pending", content: null, error: null },
    ],
  }));
  // A same-named book with different bytes must remain hidden.
  writeFileSync(resolve(checkpointDir, "checkpoint-other-book.json"), JSON.stringify({
    job_id: "checkpoint-other-book",
    source_hash: "different-book-hash",
    source_path: sourcePath,
    chapters: [],
  }));

  const port = await getFreePort();
  cdpUrl = `http://localhost:${port}`;

  appProcess = spawn(APP_PATH, [], {
    env: {
      ...process.env,
      BABEL_EBOOK_E2E_CDP_PORT: String(port),
      BABEL_EBOOK_E2E_UI_LANGUAGE: "en",
      BABEL_EBOOK_E2E_CHECKPOINT_DIR: checkpointDir,
      BABEL_EBOOK_E2E_SOURCE: sourcePath,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  appProcess.stdout?.on("data", (data) => {
    console.log(`[app stdout] ${data.toString().trim()}`);
  });
  appProcess.stderr?.on("data", (data) => {
    console.error(`[app stderr] ${data.toString().trim()}`);
  });

  const ready = await waitForCdp(cdpUrl);
  if (!ready) {
    await forceKill(appProcess);
    throw new Error(`Tauri app did not expose CDP port ${port} in time`);
  }
});

test.afterAll(async () => {
  await forceKill(appProcess);
  if (checkpointDir) rmSync(checkpointDir, { recursive: true, force: true });
});

test("loads persisted checkpoints and allows selecting a resume record", async () => {
  const browser = await chromium.connectOverCDP(cdpUrl);
  const page = browser.contexts()[0].pages()[0];
  await page.getByTestId("nav-translate").click();
  const checkpoint = page.getByTestId("checkpoint-item-checkpoint-ui-test");
  await expect(checkpoint).toBeVisible({ timeout: 15000 });
  await expect(checkpoint).toContainText("Dead Mountain checkpoint fixture.epub");
  await expect(page.getByTestId("checkpoint-item-checkpoint-other-book")).toHaveCount(0);
  await checkpoint.click();
  await expect(page.getByTestId("clear-resume-selection")).toBeVisible();
  await page.getByTestId("clear-resume-selection").click();
  await page.locator(".file-row-source .icon-button").click();
  await expect(page.getByText("Select a source EPUB to see matching resume records.")).toBeVisible();
  await expect(page.locator(".checkpoint-item")).toHaveCount(0);
  await browser.close();
});

test("shows completed count and an early failure while another chapter is still running", async () => {
  const browser = await chromium.connectOverCDP(cdpUrl);
  const page = browser.contexts()[0].pages()[0];
  await page.getByTestId("nav-logs").click();
  // Use the actual Tauri event channel, without a translator or paid API call.
  await page.evaluate(async () => {
    const runtime = (window as unknown as {
      __TAURI_INTERNALS__: { invoke: (command: string, args: Record<string, unknown>) => Promise<unknown> };
    }).__TAURI_INTERNALS__;
    for (const payload of [
      { Started: { total: 3 } },
      { ChapterStarted: { index: 0, href: "r02-slow.xhtml" } },
      { ChapterFinished: { index: 1, href: "r02-fast.xhtml" } },
      { Failed: { index: 2, href: "r02-failed.xhtml", error: "r02 offline fixture failure" } },
    ]) {
      await runtime.invoke("plugin:event|emit", { event: "translation_progress", payload });
    }
  });
  await expect(page.locator(".log-entry").filter({ hasText: "Finished: r02-fast.xhtml (1/3)" })).toBeVisible();
  await expect(page.locator(".log-entry.error").filter({ hasText: "r02 offline fixture failure" })).toBeVisible();
  await expect(page.locator(".log-entry").filter({ hasText: "Finished: r02-slow.xhtml" })).toHaveCount(0);
  await browser.close();
});

test("navigates through all settings tabs and persists changes", async () => {
  test.setTimeout(120000);
  const browser = await chromium.connectOverCDP(cdpUrl);
  const context = browser.contexts()[0];
  const page = context.pages()[0];
  page.on("console", (msg) => {
    console.log(`[browser console] ${msg.type()}: ${msg.text()}`);
  });

  await expect(page.getByTestId("nav-translate")).toBeVisible({ timeout: 10000 });
  await page.getByTestId("nav-settings").click();

  // Each tab should render its own panel.
  const tabs = [
    { id: "compute", heading: "Providers" },
    { id: "model", heading: "Model" },
    { id: "translation", heading: "Translation Options" },
    { id: "prompts", heading: "Prompts" },
    { id: "output", heading: "Output & Files" },
    { id: "queue", heading: "Task Queue" },
    { id: "general", heading: "General" },
  ];

  for (const tab of tabs) {
    await page.getByTestId(`settings-tab-${tab.id}`).click();
    await expect(
      page.getByRole("heading", { name: tab.heading, exact: true })
    ).toBeVisible({ timeout: 10000 });
  }

  // Change max_input_tokens on the Model tab and wait for autosave debounce.
  await page.getByTestId("settings-tab-model").click();
  const maxInputTokens = page.locator('label:has-text("Max Input Tokens") input');
  await expect(maxInputTokens).toBeVisible();
  await maxInputTokens.fill("1234");
  await maxInputTokens.blur();
  await page.getByTestId("settings-tab-prompts").click();
  await page.getByTestId("glossary-add").click();
  await page.getByTestId("glossary-term-0").fill("Mercer");
  await page.getByTestId("glossary-translation-0").fill("默瑟");
  await page.getByTestId("glossary-context-0").fill("Character surname");
  await page.getByTestId("glossary-context-0").blur();
  await page.waitForTimeout(700);

  // Reload the webview and verify the persisted value.
  await page.reload();
  await expect(page.getByTestId("nav-translate")).toBeVisible({ timeout: 10000 });
  await page.getByTestId("nav-settings").click();
  await page.getByTestId("settings-tab-model").click();
  await expect(maxInputTokens).toHaveValue("1234");
  await page.getByTestId("settings-tab-prompts").click();
  await expect(page.getByTestId("glossary-term-0")).toHaveValue("Mercer");
  await expect(page.getByTestId("glossary-translation-0")).toHaveValue("默瑟");
  await expect(page.getByTestId("glossary-context-0")).toHaveValue("Character surname");
  await page.getByTestId("glossary-remove-0").click();
  await expect(page.getByTestId("glossary-entry")).toHaveCount(0);

  // Queue tab: invalid concurrency shows an inline error and clamps on blur.
  await page.getByTestId("settings-tab-queue").click();
  const concurrencyInput = page.locator('label:has-text("Concurrency") input');
  await expect(concurrencyInput).toBeVisible();
  await concurrencyInput.fill("0");
  await expect(page.locator("#error-concurrency")).toBeVisible();
  await concurrencyInput.blur();
  await expect(page.locator("#error-concurrency")).not.toBeVisible();
  await expect(concurrencyInput).toHaveValue("1");

  await browser.close();
});


test("About shows the custom program revision", async () => {
  const browser = await chromium.connectOverCDP(cdpUrl);
  const page = browser.contexts()[0].pages()[0];
  await page.getByRole("button", { name: "About", exact: true }).click();
  await expect(page.locator(".about-page")).toContainText("R02");
  await browser.close();
});
