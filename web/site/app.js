import { ConversionWorker } from "./client.js";
import { basename, fileSize, noteFile, outputName, uniqueNames, zipFile } from "./files.js";
import { PdfViewer } from "./pdf.js";

const picker = document.querySelector("#files");
const drop = document.querySelector("#drop");
const format = document.querySelector("#format");
const section = document.querySelector("#results-section");
const list = document.querySelector("#results");
const message = document.querySelector("#message");
const downloadAll = document.querySelector("#download-all");
const viewer = new PdfViewer();
const converter = new ConversionWorker();
let items = [];
let selected = null;
let epoch = 0;
let running = null;
let zipping = false;
const archives = new Map();

function say(text = "") {
  message.textContent = text;
  message.hidden = !text;
}

function release(item) {
  if (item.url) URL.revokeObjectURL(item.url);
  if (item.pdfUrl && item.pdfUrl !== item.url) URL.revokeObjectURL(item.pdfUrl);
  item.url = item.pdfUrl = item.bytes = item.pdfBytes = null;
}

function updateActions() {
  section.hidden = items.length === 0;
  const notes = items.filter((item) => !item.isArchive && noteFile.test(item.file.name));
  const complete = notes.filter((item) => ["done", "failed"].includes(item.status)).length;
  document.querySelector("#file-count").textContent = notes.length ? `${complete} / ${notes.length}` : items.length;
  const busy = items.some((item) => ["queued", "converting", "reading"].includes(item.status));
  const progress = document.querySelector("#progress");
  progress.hidden = !busy;
  progress.max = notes.length || 1;
  progress.value = complete;
  downloadAll.hidden = items.filter((item) => item.status === "done").length < 2;
  downloadAll.disabled = zipping || busy;
}

function render(item) {
  const element = item.element;
  element.className = `file-item ${item.status}${selected === item ? " active" : ""}`;
  element.replaceChildren();
  const name = document.createElement("span");
  name.className = "file-name";
  name.textContent = name.title = item.relativePath || item.file.name;
  const status = document.createElement("span");
  status.className = "file-status";
  status.setAttribute("role", "status");
  status.textContent = { reading: "Reading ZIP…", queued: "Waiting", converting: "Converting…", failed: item.error, done: item.bytes && `${item.format.toUpperCase()} · ${fileSize(item.bytes.length)}` }[item.status];
  element.append(name, status);
  if (item.status === "done") {
    const actions = document.createElement("div");
    actions.className = "file-actions";
    const preview = document.createElement("button");
    preview.type = "button";
    preview.textContent = "Preview";
    preview.title = "Preview";
    preview.setAttribute("aria-label", `Preview ${item.file.name}`);
    preview.onclick = () => select(item, true);
    const download = document.createElement("a");
    download.href = item.url;
    download.download = item.outputName;
    download.textContent = "Download";
    download.title = `Download ${item.format.toUpperCase()}`;
    download.setAttribute("aria-label", `Download ${item.outputName}`);
    actions.append(preview, download);
    element.append(actions);
  }
  updateActions();
}

async function select(item, scroll = false) {
  const previous = selected;
  selected = item;
  if (previous) render(previous);
  render(item);
  await viewer.open(item);
  if (scroll && selected === item) document.querySelector(".preview").scrollIntoView({ block: "start" });
}

function nameOutputs() {
  const valid = items.filter((item) => noteFile.test(item.file.name));
  const names = uniqueNames(valid.map((item) => outputName(item.relativePath || item.file.name, item.format)));
  valid.forEach((item, index) => {
    item.outputPath = names[index];
    item.outputName = basename(names[index]);
    item.pdfName = item.outputName.replace(/\.(pdf|svg)$/i, ".pdf");
  });
  if (selected) document.querySelector("#download-preview").download = selected.pdfName;
}

function openArchive(file, version) {
  const cached = archives.get(file);
  if (!cached || cached.generation !== converter.generation) {
    const job = (async () => {
      const buffer = await file.arrayBuffer();
      if (version !== epoch) throw new Error("Cancelled");
      return converter.request({ type: "open-zip", buffer }, [buffer]);
    })();
    archives.set(file, { generation: converter.generation, job });
  }
  return archives.get(file).job;
}

async function importArchive(item, version) {
  try {
    const result = await openArchive(item.file, version);
    if (version !== epoch || !items.includes(item)) return;
    if (!result.names.length) throw new Error("No .note or .ntb files found in ZIP");
    const imported = result.names.map((path, archiveIndex) => ({
      file: { name: basename(path) }, relativePath: path, archiveFile: item.file, archiveIndex,
      format: format.value, element: document.createElement("li"), status: "queued",
    }));
    const fragment = document.createDocumentFragment();
    imported.forEach((entry) => fragment.append(entry.element));
    item.element.replaceWith(fragment);
    items.splice(items.indexOf(item), 1, ...imported);
    nameOutputs();
    items.forEach(render);
    pump();
  } catch (error) {
    if (version !== epoch) return;
    archives.delete(item.file);
    item.status = "failed";
    item.error = error.message;
    render(item);
  }
}

function addFiles(files) {
  say();
  for (const file of files) {
    const valid = noteFile.test(file.name);
    const isArchive = zipFile.test(file.name);
    const item = { file, isArchive, format: format.value, element: document.createElement("li"), status: isArchive ? "reading" : valid ? "queued" : "failed", error: "Choose a .note, .ntb, or .zip file" };
    items.push(item);
    list.append(item.element);
    if (isArchive) importArchive(item, epoch);
  }
  nameOutputs();
  items.forEach(render);
  pump();
}

async function pump() {
  if (running) return;
  const token = {};
  const version = epoch;
  running = token;
  try {
    while (version === epoch) {
      const item = items.find((candidate) => candidate.status === "queued");
      if (!item) break;
      item.status = "converting";
      render(item);
      try {
        let request;
        if (item.archiveFile) {
          const archive = await openArchive(item.archiveFile, version);
          if (version !== epoch) break;
          request = converter.request({ type: "convert", archiveId: archive.archiveId, archiveIndex: item.archiveIndex, format: item.format });
        } else {
          const buffer = await item.file.arrayBuffer();
          if (version !== epoch) break;
          request = converter.request({ type: "convert", name: item.file.name, format: item.format, buffer }, [buffer]);
        }
        const result = await request;
        if (version !== epoch) break;
        item.bytes = result.bytes;
        item.pdfBytes = result.pdfBytes;
        item.url = URL.createObjectURL(new Blob([item.bytes], { type: item.format === "pdf" ? "application/pdf" : "image/svg+xml" }));
        item.pdfUrl = item.format === "pdf" ? item.url : URL.createObjectURL(new Blob([item.pdfBytes], { type: "application/pdf" }));
        item.status = "done";
        render(item);
        if (!selected) await select(item);
      } catch (error) {
        if (version !== epoch) break;
        item.status = "failed";
        item.error = error.message;
        render(item);
      }
    }
  } finally {
    if (running === token) running = null;
    updateActions();
  }
}

function reset() {
  epoch++;
  running = null;
  selected = null;
  zipping = false;
  converter.reset();
  archives.clear();
  viewer.clear();
  items.forEach(release);
  say();
}

picker.onchange = () => { addFiles(picker.files); picker.value = ""; };
for (const type of ["dragenter", "dragover"]) {
  drop.addEventListener(type, (event) => { event.preventDefault(); drop.classList.add("dragging"); });
}
drop.addEventListener("dragleave", (event) => {
  if (!drop.contains(event.relatedTarget)) drop.classList.remove("dragging");
});
drop.addEventListener("drop", (event) => {
  event.preventDefault();
  drop.classList.remove("dragging");
  addFiles(event.dataTransfer.files);
});
// Dropping outside the picker should not navigate away from converted files.
window.addEventListener("dragover", (event) => event.preventDefault());
window.addEventListener("drop", (event) => event.preventDefault());

document.querySelector("#clear").onclick = () => {
  reset();
  items = [];
  list.replaceChildren();
  updateActions();
};

format.onchange = () => {
  reset();
  for (const item of items) {
    if (noteFile.test(item.file.name)) { item.status = "queued"; item.format = format.value; }
    if (item.isArchive && item.status === "reading") importArchive(item, epoch);
  }
  nameOutputs();
  items.forEach(render);
  pump();
};

downloadAll.onclick = async () => {
  const version = epoch;
  const done = items.filter((item) => item.status === "done");
  if (done.length < 2) return;
  zipping = true;
  updateActions();
  try {
    const entries = done.map((item) => ({ name: item.outputPath, bytes: item.bytes }));
    const result = await converter.request({ type: "zip", entries });
    if (version !== epoch) return;
    const url = URL.createObjectURL(new Blob([result.bytes], { type: "application/zip" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = "Notability notes.zip";
    document.body.append(link);
    link.click();
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  } catch (error) {
    if (version === epoch) say(error.message);
  } finally {
    if (version === epoch) { zipping = false; updateActions(); }
  }
};
