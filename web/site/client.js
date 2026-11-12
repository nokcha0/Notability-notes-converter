import { rasterizeBackgrounds } from "./pdf.js";

export class ConversionWorker {
  constructor() {
    this.pending = new Map();
    this.sequence = 0;
    this.generation = 0;
    this.worker = null;
  }

  start() {
    const worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
    this.worker = worker;
    worker.onmessage = async ({ data }) => {
      if (worker !== this.worker) return;
      if (data.type === "backgrounds") {
        try {
          const pngs = await rasterizeBackgrounds(data.backgrounds);
          if (worker === this.worker) worker.postMessage({ type: "backgrounds", id: data.id, pngs }, pngs.map((png) => png.buffer));
        } catch (error) {
          if (worker === this.worker) worker.postMessage({ type: "backgrounds", id: data.id, error: error.message });
        }
        return;
      }
      const job = this.pending.get(data.id);
      this.pending.delete(data.id);
      if (data.trapped) this.reset("Converter restarted");
      if (data.error) job?.reject(new Error(data.error));
      else job?.resolve(data);
    };
    worker.onerror = (event) => {
      event.preventDefault();
      if (worker === this.worker) this.reset(event.message || "Could not start converter");
    };
    worker.onmessageerror = () => this.reset("Could not read converter response");
  }

  reset(message = "Cancelled") {
    this.generation++;
    this.worker?.terminate();
    this.worker = null;
    for (const job of this.pending.values()) job.reject(new Error(message));
    this.pending.clear();
  }

  request(data, transfer = []) {
    if (!this.worker) this.start();
    const id = ++this.sequence;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      try {
        this.worker.postMessage({ ...data, id }, transfer);
      } catch (error) {
        this.pending.delete(id);
        reject(error);
      }
    });
  }
}
