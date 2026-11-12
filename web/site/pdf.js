import { getDocument, GlobalWorkerOptions } from "./vendor/pdfjs/pdf.mjs";

GlobalWorkerOptions.workerSrc = new URL("./vendor/pdfjs/pdf.worker.mjs", import.meta.url).href;

export function loadPdf(bytes) {
  return getDocument({
    data: bytes.slice(), // PDF.js transfers ownership to its worker.
    cMapUrl: new URL("./vendor/pdfjs/cmaps/", import.meta.url).href,
    cMapPacked: true,
    standardFontDataUrl: new URL("./vendor/pdfjs/standard_fonts/", import.meta.url).href,
    wasmUrl: new URL("./vendor/pdfjs/wasm/", import.meta.url).href,
    iccUrl: new URL("./vendor/pdfjs/iccs/", import.meta.url).href,
    isEvalSupported: false,
  });
}

export async function rasterizeBackgrounds({ assets, pages }) {
  const pngs = [];
  const tasks = new Map();
  try {
    for (const { assetIndex, pageIndex } of pages) {
      if (!tasks.has(assetIndex)) tasks.set(assetIndex, loadPdf(assets[assetIndex]));
      const task = tasks.get(assetIndex);
      const pdf = await task.promise;
      const page = await pdf.getPage(pageIndex + 1);
      const viewport = page.getViewport({ scale: 2 }); // Matches native 144 dpi.
      const canvas = document.createElement("canvas");
      canvas.width = Math.ceil(viewport.width);
      canvas.height = Math.ceil(viewport.height);
      try {
        await page.render({ canvasContext: canvas.getContext("2d"), viewport }).promise;
        const blob = await new Promise((resolve) => canvas.toBlob(resolve, "image/png"));
        if (!blob) throw new Error("Could not render PDF background");
        pngs.push(new Uint8Array(await blob.arrayBuffer()));
      } finally {
        canvas.width = canvas.height = 0;
      }
    }
  } finally {
    await Promise.all([...tasks.values()].map((task) => task.destroy()));
  }
  return pngs;
}

export class PdfViewer {
  constructor() {
    this.viewer = document.querySelector("#viewer");
    this.preview = document.querySelector(".preview");
    this.empty = document.querySelector("#preview-empty");
    this.area = document.querySelector("#page-area");
    this.container = document.querySelector("#pdf-pages");
    this.pageCount = document.querySelector("#page-count");
    this.zoom = document.querySelector("#zoom");
    this.message = document.querySelector("#viewer-message");
    this.generation = 0;
    this.revision = 0;
    this.pages = [];
    this.renderSerial = Promise.resolve();
    this.zoom.onchange = () => this.layout();
    new ResizeObserver(() => {
      const width = this.area.clientWidth;
      if (!this.pdf || !width || width === this.lastWidth) return;
      this.lastWidth = width;
      clearTimeout(this.resizeTimer);
      this.resizeTimer = setTimeout(() => {
        if (this.zoom.value === "fit") this.layout();
      }, 100);
    }).observe(this.area);
  }

  async clear() {
    const generation = ++this.generation;
    this.revision++;
    clearTimeout(this.resizeTimer);
    this.observer?.disconnect();
    this.pages.forEach((page) => this.release(page));
    this.pages = [];
    this.container.replaceChildren();
    this.pdf = null;
    const task = this.loadingTask;
    this.loadingTask = null;
    this.viewer.hidden = true;
    this.empty.hidden = false;
    this.preview.classList.remove("has-document");
    this.pageCount.textContent = "";
    document.querySelector("#download-preview").removeAttribute("href");
    if (task) await task.destroy();
    return generation;
  }

  async open(item) {
    const generation = await this.clear();
    if (generation !== this.generation) return;
    this.viewer.hidden = false;
    this.empty.hidden = true;
    this.preview.classList.add("has-document");
    this.message.hidden = true;
    const name = document.querySelector("#preview-name");
    name.textContent = name.title = item.relativePath || item.file.name;
    const download = document.querySelector("#download-preview");
    download.href = item.pdfUrl;
    download.download = item.pdfName;
    this.area.scrollTop = this.area.scrollLeft = 0;
    const task = loadPdf(item.pdfBytes);
    this.loadingTask = task;
    this.pageCount.textContent = "Loading…";
    try {
      const pdf = await task.promise;
      const first = await pdf.getPage(1);
      if (generation !== this.generation) return;
      this.pdf = pdf;
      const base = first.getViewport({ scale: 1 });
      this.pageCount.textContent = `${pdf.numPages} ${pdf.numPages === 1 ? "page" : "pages"}`;
      const fragment = document.createDocumentFragment();
      for (let number = 1; number <= pdf.numPages; number++) {
        const element = document.createElement("div");
        element.className = "pdf-page";
        element.dataset.page = number;
        element.setAttribute("role", "group");
        element.setAttribute("aria-label", `Page ${number} of ${pdf.numPages}`);
        const canvas = document.createElement("canvas");
        canvas.className = "pdf-canvas";
        canvas.width = canvas.height = 0;
        canvas.setAttribute("aria-label", `PDF page ${number}`);
        const placeholder = document.createElement("span");
        placeholder.className = "page-placeholder";
        placeholder.textContent = `Page ${number}`;
        placeholder.setAttribute("aria-hidden", "true");
        element.append(canvas, placeholder);
        fragment.append(element);
        this.pages.push({ number, element, canvas, base, generation, nearby: false, renderedRevision: -1 });
      }
      this.container.append(fragment);
      this.observer = new IntersectionObserver((entries) => {
        for (const entry of entries) {
          const page = this.pages[Number(entry.target.dataset.page) - 1];
          if (!page || page.element !== entry.target) continue;
          page.nearby = entry.isIntersecting;
          if (page.nearby) this.queueRender(page);
          else this.release(page);
        }
      }, { root: this.area, rootMargin: "600px 0px" });
      this.pages.forEach((page) => this.observer.observe(page.element));
      await this.layout();
    } catch (error) {
      if (generation === this.generation) this.showError(error);
    }
  }

  metrics(page) {
    const style = getComputedStyle(this.area);
    const available = this.area.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
    const scale = this.zoom.value === "fit" ? Math.max(.1, available / page.base.width) : Number(this.zoom.value);
    return { scale, width: page.base.width * scale, height: page.base.height * scale };
  }

  layout() {
    if (!this.pdf || !this.pages.length) return Promise.resolve();
    const padding = parseFloat(getComputedStyle(this.area).paddingTop);
    const scrollTop = this.area.scrollTop;
    // Keep the same page and position when zooming or rotating a phone.
    const anchor = this.pages.find((page) => padding + page.element.offsetTop + page.element.offsetHeight > scrollTop) || this.pages.at(-1);
    const fraction = anchor.element.offsetHeight
      ? Math.max(0, (scrollTop - padding - anchor.element.offsetTop) / anchor.element.offsetHeight) : 0;
    this.revision++;
    this.lastWidth = this.area.clientWidth;
    for (const page of this.pages) {
      this.release(page);
      const { width, height } = this.metrics(page);
      page.element.style.width = `${width}px`;
      page.element.style.height = `${height}px`;
    }
    this.area.scrollTop = scrollTop ? padding + anchor.element.offsetTop + fraction * anchor.element.offsetHeight : 0;
    if (this.zoom.value === "fit") this.area.scrollLeft = 0;
    const area = this.area.getBoundingClientRect();
    for (const page of this.pages) {
      const bounds = page.element.getBoundingClientRect();
      page.nearby = bounds.bottom > area.top - 600 && bounds.top < area.bottom + 600;
      if (page.nearby) this.queueRender(page);
    }
    return this.renderSerial;
  }

  release(page) {
    page.task?.cancel();
    page.canvas.width = page.canvas.height = 0;
    page.renderedRevision = -1;
    delete page.element.dataset.rendered;
    page.proxy?.cleanup();
  }

  queueRender(page) {
    const generation = this.generation;
    const revision = this.revision;
    if (!this.pdf || !page.nearby || page.renderedRevision === revision || page.queuedRevision === revision) return;
    const pdf = this.pdf;
    page.queuedRevision = revision;
    const current = () => generation === this.generation && revision === this.revision && page.nearby;
    this.renderSerial = this.renderSerial.catch(() => {}).then(async () => {
      let task;
      let cancelled = false;
      try {
        if (!current()) return;
        const proxy = await pdf.getPage(page.number);
        if (!current()) return;
        page.proxy = proxy;
        page.base = proxy.getViewport({ scale: 1 });
        const { scale, width, height } = this.metrics(page);
        const viewport = proxy.getViewport({ scale });
        page.element.style.width = `${width}px`;
        page.element.style.height = `${height}px`;
        // Bound canvas memory on high-DPI screens and at larger zoom levels.
        const pixels = Math.min(window.devicePixelRatio || 1, 2, Math.sqrt(16_000_000 / (width * height)));
        page.canvas.width = Math.ceil(width * pixels);
        page.canvas.height = Math.ceil(height * pixels);
        task = proxy.render({
          canvasContext: page.canvas.getContext("2d"),
          viewport,
          transform: pixels === 1 ? null : [pixels, 0, 0, pixels, 0, 0],
        });
        page.task = task;
        await task.promise;
        if (!current()) return;
        page.renderedRevision = revision;
        page.element.dataset.rendered = "true";
      } catch (error) {
        cancelled = error.name === "RenderingCancelledException";
        if (!cancelled && current()) this.showError(error);
      } finally {
        if (page.task === task) page.task = null;
        if (page.queuedRevision === revision) page.queuedRevision = null;
        if (cancelled && current()) this.queueRender(page);
      }
    });
  }

  showError(error) {
    this.message.textContent = `Preview unavailable: ${error.message}`;
    this.message.hidden = false;
  }
}
