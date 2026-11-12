import { test } from "node:test";
import assert from "node:assert/strict";
import { basename, noteFile, outputName, uniqueNames, zipFile } from "../site/files.js";

test("accepts both note formats without changing the stem", () => {
  assert.ok(noteFile.test("My Note.NTB"));
  assert.ok(noteFile.test("My Note.note"));
  assert.equal(outputName("My.Note.NTB", "pdf"), "My.Note.pdf");
  assert.equal(outputName("My Note.note", "svg"), "My Note.svg");
  assert.ok(!noteFile.test("Note.pdf"));
});

test("archive output names preserve folders and avoid duplicates within each folder", () => {
  assert.ok(zipFile.test("Notes.ZIP"));
  assert.ok(!zipFile.test("Note.note"));
  const names = uniqueNames(["Course/First.pdf", "Other/First.pdf", "Course/first.pdf"]);
  assert.deepEqual(names, ["Course/First.pdf", "Other/First.pdf", "Course/first (2).pdf"]);
  assert.equal(basename(outputName("Course/First.note", "svg")), "First.svg");
  assert.deepEqual(uniqueNames(["Note.pdf", "Note.pdf/Inside.pdf"]), ["Note (2).pdf", "Note.pdf/Inside.pdf"]);
});

test("batch download names cannot collide with existing numbered names", () => {
  const names = uniqueNames(["Note.pdf", "note.pdf", "Note (2).pdf", "Note.pdf", "Note.svg"]);
  assert.deepEqual(names, ["Note.pdf", "note (2).pdf", "Note (2) (2).pdf", "Note (3).pdf", "Note.svg"]);
  assert.equal(new Set(names.map((name) => name.toLowerCase())).size, names.length);
});
