import init, { Archive, Converter, InputArchive } from "./pkg/notability_web.js";

// Catch startup failure immediately, even if a file has not been chosen yet.
const ready = init().then(() => ({ ok: true }), (error) => ({ error }));
const backgroundJobs = new Map();
const inputs = new Map();

self.onmessage = async ({ data }) => {
  if (data.type === "backgrounds") {
    const job = backgroundJobs.get(data.id);
    backgroundJobs.delete(data.id);
    if (data.error) job?.reject(new Error(data.error));
    else job?.resolve(data.pngs);
    return;
  }
  let converter;
  let archive;
  try {
    const status = await ready;
    if (status.error) throw new Error(`Could not load converter: ${status.error.message}`);
    if (data.type === "open-zip") {
      const input = new InputArchive(new Uint8Array(data.buffer));
      const names = input.names();
      inputs.set(data.id, input);
      self.postMessage({ id: data.id, archiveId: data.id, names });
      return;
    }
    if (data.type === "zip") {
      archive = new Archive();
      for (const entry of data.entries) archive.add(entry.name, entry.bytes);
      const finishing = archive;
      archive = null; // finish consumes the Rust object, including on failure.
      const bytes = finishing.finish();
      self.postMessage({ id: data.id, bytes }, [bytes.buffer]);
      return;
    }
    if (data.type !== "convert") throw new Error("Unknown conversion request");
    if (data.archiveId !== undefined) {
      const input = inputs.get(data.archiveId);
      if (!input) throw new Error("ZIP archive is no longer available");
      converter = input.converter(data.archiveIndex);
    } else {
      converter = new Converter(new Uint8Array(data.buffer), data.name);
    }
    const pdfBytes = converter.pdf();
    let bytes;
    if (data.format === "svg") {
      const count = converter.prepare_svg();
      if (count) {
        const assets = Array.from({ length: converter.background_asset_count() }, (_, index) => converter.background_pdf(index));
        const pages = Array.from({ length: count }, (_, index) => ({
          assetIndex: converter.background_asset(index), pageIndex: converter.background_page(index),
        }));
        const waiting = new Promise((resolve, reject) => backgroundJobs.set(data.id, { resolve, reject }));
        self.postMessage({ type: "backgrounds", id: data.id, backgrounds: { assets, pages } }, assets.map((bytes) => bytes.buffer));
        const pngs = await waiting;
        pngs.forEach((png, index) => converter.set_background(index, png));
      }
      bytes = converter.svg();
    } else if (data.format === "pdf") {
      bytes = pdfBytes;
    } else {
      throw new Error("Unsupported output format");
    }
    const transfers = bytes === pdfBytes ? [bytes.buffer] : [bytes.buffer, pdfBytes.buffer];
    self.postMessage({ id: data.id, bytes, pdfBytes }, transfers);
  } catch (error) {
    self.postMessage({ id: data.id, error: error.message ?? String(error), trapped: error instanceof WebAssembly.RuntimeError });
  } finally {
    converter?.free();
    archive?.free();
  }
};
