export const noteFile = /\.(note|ntb)$/i;
export const zipFile = /\.zip$/i;

export function basename(path) {
  return path.split("/").pop();
}

export function outputName(name, format) {
  return name.replace(noteFile, "") + "." + format;
}

// Reserve the whole name so an existing "Note (2).pdf" cannot be overwritten.
export function uniqueNames(names) {
  const used = new Set();
  const folders = new Set();
  for (const name of names) {
    const parts = name.split("/");
    for (let end = 1; end < parts.length; end++) {
      folders.add(parts.slice(0, end).join("/").toLowerCase());
    }
  }
  return names.map((name) => {
    const dot = name.lastIndexOf(".");
    const stem = dot < 0 ? name : name.slice(0, dot);
    const extension = dot < 0 ? "" : name.slice(dot);
    let candidate = name;
    let number = 2;
    while (used.has(candidate.toLowerCase()) || folders.has(candidate.toLowerCase())) {
      candidate = `${stem} (${number++})${extension}`;
    }
    used.add(candidate.toLowerCase());
    return candidate;
  });
}

export function fileSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
