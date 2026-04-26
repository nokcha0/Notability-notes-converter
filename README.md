# Notability `.note` Converter

Converts `.note` files to `.pdf` by default, or optionally to `.svg`.

I originally made this for personal use because I couldn't find a converter online. I hope it is useful to others as well.

## Where is this useful?

- Convert `.note` files to other formats in bulk (high speed is a plus)
- View `.note` files on a machine that does not have Notability installed
- Switch platforms by converting `.note` files to `.svg` (SVGs can be imported into other note-taking apps as editable files)

## How to use

Requirement: Rust

Put your exported `.note` files in `input/`, then run this from the repository root:

```bash
cargo run
```

This converts everything under `input/` into `output/`.
Nested folder structures are preserved.

Example:

```text
.
├── input/
│   ├── example1.note
│   └── example_folder/
│       └── example2.note
└── output/
    ├── example1.pdf
    └── example_folder/
        └── example2.pdf
```

## Optional flags: format and input/output folders

```bash
cargo run -- --format svg --input path/to/input --output path/to/output
```

## Analysis of the `.note` file format

Thanks to [Julia Evans' blog](https://jvns.ca/blog/2018/03/31/reverse-engineering-notability-format/), I was able to quickly understand how the file format works.

A `.note` file is a ZIP archive containing:

- `Session.plist`: the main document object graph
- `PDFs/`: imported PDF source files
- image assets referenced by embedded figures
- thumbnail PNGs used as a fallback for page aspect ratios

`Session.plist` is not a flat plist. It is an Apple keyed archive containing `$objects` and `$top`, so decoding it requires following UID references through the object table.

The main content is stored in:

- `richText`: the main content container
- `richText -> reflowState`: contains layout information, such as `pageWidthInDocumentCoordsKey`
- `richText -> attributedString`: stores the plain text and style subranges
- `richText -> mediaObjects`: stores placed images, including their document coordinates, size, and z-order
- `richText -> pageLayoutArray`: maps note pages to imported PDF files and their source page indices
- `richText -> Handwriting Overlay -> SpatialHash`: stores pen stroke payloads

The handwriting payload in `SpatialHash` mostly consists of little-endian binary arrays:

- `curvespoints`: `f32` pairs representing stroke point coordinates
- `curvesnumpoints`: `i32` point counts for each stroke
- `curveswidth`: base stroke widths stored as `f32`
- `curvesforces`: pressure samples stored as `f32`
- `curvesfractionalwidths`: width multipliers stored as `f32`
- `curvescolors`: RGBA bytes
- `curvesstyles`: per-stroke tool/style identifiers

Many strokes follow a cubic Bézier layout where the point count follows `1 + 3n`, so the data is not simply a plain polyline list.

Page geometry is reconstructed from:

- the explicit document width from `reflowState`
- imported PDF page boxes, when the note is PDF-backed
- the thumbnail aspect ratio as a fallback when no embedded PDF page defines the height
- paper attributes, such as line style, paper size, and sizing behavior

This is enough to reconstruct the sample notes.
