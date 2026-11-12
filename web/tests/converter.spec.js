import { test, expect } from "@playwright/test";
import { fileURLToPath } from "node:url";
import { readFile } from "node:fs/promises";
import { execFileSync } from "node:child_process";

const root = fileURLToPath(new URL("../../", import.meta.url));
const samples = ["NOTE Note Apr 26, 2026.note", "NTB Note Apr 26, 2026.ntb"];

test("converts both formats locally, previews pages, and downloads PDF and ZIP", async ({ page }) => {
  const errors = [];
  const externalRequests = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    if (!request.url().startsWith("http://127.0.0.1:") && !request.url().startsWith("blob:")) externalRequests.push(request.url());
  });
  await page.goto("./");
  await page.locator("#files").setInputFiles(samples.map((name) => `${root}/input/${name}`));
  await expect(page.locator(".file-item.done")).toHaveCount(2);
  await expect(page.locator('.pdf-page[data-page="1"]')).toHaveAttribute("data-rendered", "true");
  await expect(page.locator(".pdf-page")).toHaveCount(2);
  await expect(page.locator("#page-count")).toHaveText("2 pages");
  await page.locator("#page-area").evaluate((area) => { area.scrollTop = area.scrollHeight; });
  await expect(page.locator('.pdf-page[data-page="2"]')).toHaveAttribute("data-rendered", "true");
  expect(await page.locator("#page-area").evaluate((area) => area.scrollTop)).toBeGreaterThan(0);
  await page.locator("#zoom").selectOption("1.5");
  await expect(page.locator("#viewer-message")).toBeHidden();
  await page.getByRole("button", { name: `Preview ${samples[1]}`, exact: true }).click();
  await expect(page.locator('.pdf-page[data-page="1"]')).toHaveAttribute("data-rendered", "true");
  await page.locator("#zoom").selectOption("fit");

  // Compare actual browser output with native PDFs at the page operator level.
  const parity = await page.evaluate(async (names) => {
    const { loadPdf } = await import("./pdf.js");
    const results = [];
    for (const name of names) {
      const item = [...document.querySelectorAll(".file-item")].find((li) => li.querySelector(".file-name").textContent === name);
      const browserBytes = new Uint8Array(await (await fetch(item.querySelector("a[download]").href)).arrayBuffer());
      const nativeBytes = new Uint8Array(await (await fetch(`/target/browser-reference/${name.replace(/\.(note|ntb)$/, ".pdf")}`)).arrayBuffer());
      const browserTask = loadPdf(browserBytes);
      const nativeTask = loadPdf(nativeBytes);
      try {
        const [browser, native] = await Promise.all([browserTask.promise, nativeTask.promise]);
        const issues = [];
        const compare = (a, b, path = "") => {
          if (typeof a === "number" && typeof b === "number") {
            if (Math.abs(a - b) > .001) issues.push(`${path}: ${a} / ${b}`);
          } else if (a && b && typeof a === "object" && typeof b === "object") {
            if (Array.isArray(a) || ArrayBuffer.isView(a)) compare(a.length, b.length, `${path}.length`);
            const keys = Object.keys(a).filter((key) => !["cacheKey", "ref", "loadedName", "g_fontName", "bitmap", "data", "fontName"].includes(key));
            const otherKeys = Object.keys(b).filter((key) => !["cacheKey", "ref", "loadedName", "g_fontName", "bitmap", "data", "fontName"].includes(key));
            if (keys.join("\n") !== otherKeys.join("\n")) issues.push(`${path}: different fields`);
            for (const key of keys) compare(a[key], b[key], `${path}.${key}`);
          } else if (a !== b && !(typeof a === "string" && typeof b === "string" && /^g_d\d+_/.test(a) && /^g_d\d+_/.test(b))) {
            issues.push(`${path}: ${a} / ${b}`);
          }
        };
        compare(browser.numPages, native.numPages, "pages");
        for (let number = 1; number <= browser.numPages; number++) {
          const bp = await browser.getPage(number);
          const np = await native.getPage(number);
          compare(bp.view, np.view, `page ${number} bounds`);
          const bo = await bp.getOperatorList();
          const no = await np.getOperatorList();
          compare(bo.fnArray, no.fnArray, `page ${number} operators`);
          compare(bo.argsArray, no.argsArray, `page ${number} arguments`);
        }
        results.push({ name, issues: issues.slice(0, 10) });
      } finally {
        await browserTask.destroy();
        await nativeTask.destroy();
      }
    }
    return results;
  }, samples);
  expect(parity).toEqual(samples.map((name) => ({ name, issues: [] })));

  const pdfDownload = page.waitForEvent("download");
  await page.locator("#download-preview").click();
  const pdf = await pdfDownload;
  expect(pdf.suggestedFilename()).toBe("NTB Note Apr 26, 2026.pdf");
  expect((await readFile(await pdf.path())).subarray(0, 5).toString()).toBe("%PDF-");
  const zipDownload = page.waitForEvent("download");
  await page.locator("#download-all").click();
  const zip = await zipDownload;
  expect(zip.suggestedFilename()).toBe("Notability notes.zip");
  const entries = JSON.parse(execFileSync("python3", ["-c", "import sys,json,zipfile; z=zipfile.ZipFile(sys.argv[1]); print(json.dumps({n:z.read(n)[:5].decode() for n in z.namelist()}))", await zip.path()], { encoding: "utf8" }));
  expect(entries).toEqual({ "NOTE Note Apr 26, 2026.pdf": "%PDF-", "NTB Note Apr 26, 2026.pdf": "%PDF-" });
  expect(errors).toEqual([]);
  expect(externalRequests).toEqual([]);
});

test("SVG includes PDF backgrounds, keeps vector paths, and has a PDF preview", async ({ page }) => {
  await page.goto("./");
  await page.locator("#format").selectOption("svg");
  await page.locator("#files").setInputFiles([
    `${root}/target/web-fixtures/PDF background.note`, `${root}/input/${samples[1]}`,
  ]);
  await expect(page.locator(".file-item.done")).toHaveCount(2);
  const svg = await page.locator(".file-item.done a[download]").evaluateAll(async (links) => Promise.all(links.map(async (link) => ({
    name: link.download, text: await (await fetch(link.href)).text(),
  }))));
  expect(svg[0].text).toContain("data:image/png;base64,");
  expect(svg[0].text.match(/data:image\/png;base64,/g)).toHaveLength(2);
  const size = svg[0].text.match(/<svg[^>]*width="([\d.]+)" height="([\d.]+)"/);
  expect(Number(size[2]) / 2 / Number(size[1])).toBeCloseTo(180 / 260, 3);
  expect(svg[0].text).not.toContain("data:application/pdf");
  expect(svg[1].text).toContain("<path");
  expect(svg[1].name).toMatch(/\.svg$/);
  await expect(page.locator('.pdf-page[data-page="1"]')).toHaveAttribute("data-rendered", "true");
  await expect(page.locator("#viewer-message")).toBeHidden();
});

test("handles bad files, changing format, clear, and duplicate names", async ({ page }) => {
  await page.goto("./");
  await page.locator("#files").setInputFiles([
    { name: "bad.txt", mimeType: "text/plain", buffer: Buffer.from("text") },
    { name: "broken.ntb", mimeType: "application/octet-stream", buffer: Buffer.from("broken") },
    { name: "same.note", mimeType: "application/octet-stream", buffer: await readFile(`${root}/input/${samples[0]}`) },
    { name: "same.note", mimeType: "application/octet-stream", buffer: await readFile(`${root}/input/${samples[0]}`) },
  ]);
  await expect(page.locator(".file-item.failed")).toHaveCount(2);
  await expect(page.locator(".file-item.done")).toHaveCount(2);
  await expect(page.locator(".file-item.done a[download]").nth(1)).toHaveAttribute("download", "same (2).pdf");
  await page.locator("#format").selectOption("svg");
  await expect(page.locator(".file-item.done")).toHaveCount(2);
  await expect(page.locator(".file-item.done a[download]").first()).toHaveAttribute("download", "same.svg");
  await page.locator("#clear").click();
  await expect(page.locator("#results-section")).toBeHidden();
  await expect(page.locator("#viewer")).toBeHidden();
  await expect(page.locator("#preview-empty")).toBeVisible();
  await expect(page.locator("#drop")).toBeVisible();
  await page.locator("#files").setInputFiles(`${root}/input/${samples[1]}`);
  await page.locator("#clear").click();
  await page.locator("#files").setInputFiles(`${root}/input/${samples[0]}`);
  await expect(page.locator(".file-item.done")).toHaveCount(1);
  await expect(page.locator(".file-name")).toHaveText(samples[0]);
});

test("works on a narrow screen", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("./");
  await page.locator("#files").setInputFiles(`${root}/input/${samples[1]}`);
  await expect(page.locator(".file-item.done")).toHaveCount(1);
  await expect(page.locator('.pdf-page[data-page="1"]')).toHaveAttribute("data-rendered", "true");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await expect(page.locator("#viewer-message")).toBeHidden();
});

test("bulk converts a ZIP, preserves folders, isolates bad notes, and can change format", async ({ page }) => {
  const errors = [];
  const uploads = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => { if (request.method() !== "GET") uploads.push(request.url()); });
  await page.goto("./");
  // Exercise drop handling with the same archive accepted by the file picker.
  const archive = await readFile(`${root}/target/web-fixtures/bulk.zip`);
  const data = await page.evaluateHandle((base64) => {
    const bytes = Uint8Array.from(atob(base64), (ch) => ch.charCodeAt(0));
    const transfer = new DataTransfer();
    transfer.items.add(new File([bytes], "bulk.zip", { type: "application/zip" }));
    return transfer;
  }, archive.toString("base64"));
  await page.locator("#drop").dispatchEvent("drop", { dataTransfer: data });
  await data.dispose();
  await expect(page.locator(".file-item.done")).toHaveCount(4);
  await expect(page.locator(".file-item.failed")).toHaveCount(1);
  await expect(page.locator("#file-count")).toHaveText("5 / 5");
  await expect(page.locator("#progress")).toBeHidden();
  await expect(page.locator(".file-name")).toHaveText([
    "Course/First.note", "Course/Second.NTB", "Other/First.note", "Course/First.NOTE", "Course/broken.ntb",
  ]);
  const waiting = page.waitForEvent("download");
  await page.locator("#download-all").click();
  const download = await waiting;
  const contents = JSON.parse(execFileSync("python3", ["-c", "import sys,json,zipfile; z=zipfile.ZipFile(sys.argv[1]); print(json.dumps({n:z.read(n)[:5].decode() for n in z.namelist()}))", await download.path()], { encoding: "utf8" }));
  expect(contents).toEqual({
    "Course/First.pdf": "%PDF-", "Course/Second.pdf": "%PDF-",
    "Other/First.pdf": "%PDF-", "Course/First (2).pdf": "%PDF-",
  });
  await page.locator("#format").selectOption("svg");
  await expect(page.locator(".file-item.done")).toHaveCount(4);
  await expect(page.locator(".file-item.failed")).toHaveCount(1);
  await expect(page.locator(".file-item.done a[download]").first()).toHaveAttribute("download", "First.svg");
  expect(errors).toEqual([]);
  expect(uploads).toEqual([]);
});

test("handles empty and invalid ZIPs and clearing a pending archive", async ({ page }) => {
  await page.goto("./");
  await page.locator("#files").setInputFiles([
    `${root}/target/web-fixtures/empty.zip`, `${root}/target/web-fixtures/broken.zip`,
  ]);
  await expect(page.locator(".file-item.failed")).toHaveCount(2);
  await expect(page.locator(".file-status").first()).toHaveText("No .note or .ntb files found in ZIP");
  await page.locator("#clear").click();
  await page.locator("#files").setInputFiles(`${root}/target/web-fixtures/bulk.zip`);
  await page.locator("#clear").click();
  await page.locator("#files").setInputFiles(`${root}/target/web-fixtures/PDF background.note`);
  await expect(page.locator(".file-item.done")).toHaveCount(1);
  await expect(page.locator(".file-name")).toHaveText("PDF background.note");
});

test("keeps the upload and viewer controls usable at mobile and desktop widths", async ({ page }) => {
  await page.goto("./");
  await expect(page.locator(".privacy")).toHaveText("Conversion runs locally in your browser. Your files are never sent to a server.");
  for (const width of [320, 390, 768, 1280]) {
    await page.setViewportSize({ width, height: 844 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    const zone = await page.locator("#drop").boundingBox();
    expect(zone.width).toBeLessThan(width);
    expect(zone.height).toBeGreaterThanOrEqual(180);
    expect(await page.locator(".privacy").evaluate((element) => (
      element.scrollHeight <= element.clientHeight && element.scrollWidth <= element.clientWidth
      && getComputedStyle(element).overflow === "visible"
    ))).toBe(true);
  }
  await page.locator("#files").setInputFiles(`${root}/target/web-fixtures/PDF background.note`);
  await expect(page.locator('.pdf-page[data-page="1"]')).toHaveAttribute("data-rendered", "true");
  for (const width of [320, 390, 768, 1280]) {
    await page.setViewportSize({ width, height: 844 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    const heights = [];
    for (const id of ["zoom", "download-preview"]) {
      const control = await page.locator(`#${id}`).boundingBox();
      expect(control.x).toBeGreaterThanOrEqual(0);
      expect(control.x + control.width).toBeLessThanOrEqual(width);
      expect(control.height).toBeGreaterThanOrEqual(44);
      heights.push(control.height);
    }
    expect(heights[0]).toBe(heights[1]);
  }
});

test("scrolls long PDFs continuously, renders nearby pages, and keeps position when zooming", async ({ page }) => {
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto("./");
  await page.locator("#files").setInputFiles(`${root}/target/web-fixtures/Long.note`);
  await expect(page.locator(".pdf-page")).toHaveCount(64);
  await expect(page.locator('.pdf-page[data-page="1"]')).toHaveAttribute("data-rendered", "true");
  await expect(page.locator("#page-count")).toHaveText("64 pages");
  await expect(page.locator("#next, #previous")).toHaveCount(0);
  expect(await page.locator('.pdf-page[data-rendered="true"]').count()).toBeLessThan(8);
  const area = page.locator("#page-area");
  await area.scrollIntoViewIfNeeded();
  await area.evaluate((element) => { element.scrollTop = element.scrollHeight; });
  await expect(page.locator('.pdf-page[data-page="64"]')).toHaveAttribute("data-rendered", "true");
  await expect(page.locator('.pdf-page[data-page="1"]')).not.toHaveAttribute("data-rendered", "true");
  await page.locator("#zoom").selectOption("1.5");
  await expect(page.locator('.pdf-page[data-page="64"]')).toHaveAttribute("data-rendered", "true");
  expect(await area.evaluate((element) => element.scrollTop / (element.scrollHeight - element.clientHeight))).toBeGreaterThan(.95);
  await page.locator("#zoom").selectOption("fit");
  await area.evaluate((element) => { element.scrollTop = 0; });
  await expect(page.locator('.pdf-page[data-page="1"]')).toHaveAttribute("data-rendered", "true");
  await area.focus();
  await page.keyboard.press("PageDown");
  await expect.poll(() => area.evaluate((element) => element.scrollTop)).toBeGreaterThan(0);
  await expect(page.locator("#viewer-message")).toBeHidden();
  expect(errors).toEqual([]);
});

test("fits desktop panels within the window and contains scrolling at their edges", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("./");
  const note = await readFile(`${root}/target/web-fixtures/PDF background.note`);
  await page.locator("#files").setInputFiles(Array.from({ length: 5 }, (_, index) => ({
    name: `Note ${index + 1}.note`, mimeType: "application/octet-stream", buffer: note,
  })));
  await expect(page.locator(".file-item.done")).toHaveCount(5);
  const left = page.locator(".converter");
  const right = page.locator(".preview");
  const area = page.locator("#page-area");
  for (const viewport of [{ width: 1024, height: 600 }, { width: 1280, height: 720 }, { width: 1920, height: 1080 }]) {
    await page.setViewportSize(viewport);
    const upload = await left.boundingBox();
    const preview = await right.boundingBox();
    expect(upload.x + upload.width).toBeLessThan(preview.x);
    expect(upload.y).toBe(preview.y);
    expect(upload.y + upload.height).toBeLessThanOrEqual(viewport.height);
    expect(preview.y + preview.height).toBeLessThanOrEqual(viewport.height);
    expect(preview.height).toBeLessThanOrEqual(600);
    expect(preview.x + preview.width - upload.x).toBeLessThanOrEqual(1100);
    expect(await page.evaluate(() => document.scrollingElement.scrollHeight <= innerHeight)).toBe(true);
    expect(await area.evaluate((element) => getComputedStyle(element).overscrollBehaviorY)).toBe("none");
    expect(await left.evaluate((element) => getComputedStyle(element).overscrollBehaviorY)).toBe("none");
  }
  await page.setViewportSize({ width: 1280, height: 720 });
  await area.evaluate((element) => { element.scrollTop = element.scrollHeight; });
  await area.hover();
  await page.mouse.wheel(0, 1200);
  expect(await page.evaluate(() => scrollY)).toBe(0);
  await area.evaluate((element) => { element.scrollTop = 0; });
  await page.mouse.wheel(0, -1200);
  expect(await page.evaluate(() => scrollY)).toBe(0);
  await page.setViewportSize({ width: 390, height: 844 });
  const upload = await left.boundingBox();
  const preview = await right.boundingBox();
  expect(preview.y).toBeGreaterThan(upload.y + upload.height);
  expect(await page.evaluate(() => getComputedStyle(document.documentElement).overscrollBehaviorY)).toBe("none");
});
