# Notability `.note` Converter

Put your exported `.note` files under `input/`, then run this from the repo root:

```bash
cargo run
```

That converts everything under `input/` into `output/`.

## Optional Output Folder

If you want a different output folder:

```bash
cargo run -- --output my-output
```

Short form:

```bash
cargo run -- -o my-output
```

## Folder Behavior

- Input is always `input/`
- Default output is `output/`
- The output tree mirrors the input tree
- Every `.note` file becomes a `.pdf`
- Non-note files are copied as-is
- The output folder is regenerated on each run so stale files do not remain

Example:

```text
input/Semester 1/Week 1/Lecture.note
input/Semester 1/Week 1/handout.txt
input/Semester 1/Week 2/Review.note
```

becomes:

```text
output/Semester 1/Week 1/Lecture.pdf
output/Semester 1/Week 1/handout.txt
output/Semester 1/Week 2/Review.pdf
```

If `input/` does not exist yet, the first run creates it and stops so you can drop files into it.

## What It Parses

The converter treats `.note` as a ZIP bundle and reads `Session.plist` to reconstruct:

- ruled paper backgrounds
- typed text
- embedded image media objects
- imported PDF pages
- handwriting strokes

It is tuned against the sample notes in this repo and emits vector PDF output from Rust.
