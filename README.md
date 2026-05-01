# Notability `.note` Converter

Converts `.note` files to `.pdf` by default, or optionally to `.svg`.

I originally made this for personal use because I couldn't find a converter online. I hope it is useful to others as well.

Note: output is not perfectly identical to Notability’s official export. Some details, such as Apple-specific fonts and exact text layout are approximated, but for most notes this converter works pretty well.

## Where is this useful?

- Convert `.note` files to other formats in bulk (high speed is a plus)
- View `.note` files on a machine that does not have Notability installed
- Switch platforms by converting `.note` files to `.svg` (SVGs can be imported into other note-taking apps as editable files)

## How to use

Requirement: [Rust](https://rust-lang.org/tools/install/)

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

A `.note` file is a ZIP archive with a document folder inside it. The folder name does not have to match the file name, so locate `Session.plist` first and treat its parent folder as the bundle root. Typical contents are:

- `Session.plist`: the main document object graph
- `Images/`: embedded image files referenced by figure objects
- `PDFs/`: imported PDF files used as page backgrounds
- `thumb*.png`: thumbnails, useful as a fallback for page aspect ratio
- `metadata.plist`, `Recordings/`, and similar side files

`Session.plist` is an Apple keyed archive, not a plain property-list tree. The top-level plist contains `$objects` and `$top`. Values are often `UID` references into `$objects`, so a reader needs to resolve UIDs before interpreting fields. Archived arrays and dictionaries often appear as Objective-C shapes such as `NS.objects`, `NS.keys`, and `NS.bytes`.

Most useful page and drawing data is under `richText`.

Geometry fields define how document coordinates become pages:

- `richText -> reflowState -> pageWidthInDocumentCoordsKey`: document-space page width
- paper attributes: paper size, line style, and page sizing hints
- `pageLayoutArray`: imported PDF pages, whose page boxes can define the output ratio
- `thumb*.png`: fallback aspect ratio when no source PDF is available

Notability stores content in one continuous vertical document coordinate space. To convert it to pages, choose a page height from the geometry above, then split any item by:

```text
page_index = floor(document_y / page_height)
local_y = document_y - page_index * page_height
```

Text appears in two forms:

- `richText -> attributedString`: the main text stream
- `TextBlockMediaObject` entries in `mediaObjects`: positioned text boxes

The attributed string stores plain text in `stringKey` and style runs in `subRangesKey`. Useful style fields include:

- `NSFontSizeAttribute` and `NSFontNameAttribute`
- `subRangeColorCrossPlatformKey` or `subRangeColorKey`
- underline, strikethrough, and baseline offset in `subRangeOtherAttributesKey`
- list/checklist metadata such as `indent-level`, `indent-decoration-style`, `indent-decoration-number`, and `checklist-checked`

To convert text, reconstruct a per-character style map from the ranges, then lay out the text in document coordinates. This is the least exact part of the format because Notability's native font shaping and wrapping are not stored as final glyph positions.

Images are `mediaObjects` with nested figure/background/snapshot objects pointing to files in `Images/`. Important fields are:

- `documentOrigin`: document-space position
- `unscaledContentSize`: displayed size
- `rotationDegrees`: rotation; some files store radians despite the name
- `isFlippedHorizontal` and `isFlippedVertical`: independent mirror flags
- `FigureCropRectKey`: crop rectangle in source-image pixels
- `zIndex`: layer order

To convert images, read the referenced asset from `Images/`, crop it if `FigureCropRectKey` exists, then place it at `documentOrigin` with size, rotation, and flips applied around the displayed image center. If a referenced RGB JPEG is uncropped or the crop covers the entire source image, it can be embedded directly in the output PDF instead of being decoded and re-encoded.

Imported PDF backgrounds are described by `richText -> pageLayoutArray`. Each entry maps:

- note page index
- source PDF filename under `PDFs/`
- source page index inside that PDF

To convert a PDF-backed page, use the source PDF page as the base layer, scale it to the reconstructed output page box, then draw the note overlays above it.

Handwriting is stored under `richText -> Handwriting Overlay -> SpatialHash`. The main stroke payloads are little-endian binary arrays:

- `curvespoints`: `f32` x/y point pairs
- `curvesnumpoints`: `i32` point counts per curve
- `curveswidth`: base stroke widths as `f32`
- `curvesforces`: pressure samples as `f32`
- `curvesfractionalwidths`: width multipliers as `f32`
- `curvescolors`: raw RGBA bytes
- `curvesstyles`: tool/style ids
- `dashStyles`: dash/dot pattern metadata keyed by curve index
- `groupsArrays`: nested stroke groups with transforms
- `shapes`: simple shape strokes

Many curves are cubic Bezier chains: the point count follows `1 + 3n`, where each segment uses one start point and three more points. Pressure and fractional-width arrays may align to the Bezier sample count rather than the raw point count. A converter should preserve that pressure data for pen and pencil width/opacity.

Dash and dot patterns live in `dashStyles -> objectPatterns -> <curve-index> -> pattern`. Keep the original curve index when attaching dash metadata; filtering or reordering curves too early can assign dotted styles to the wrong stroke.

Grouped handwriting in `groupsArrays` has its own nested archive data plus a transform. Apply the group transform to stroke coordinates. Scale stroke width by the transform as well, otherwise grouped strokes render too thick or too thin.

Shape strokes are separate from normal raw curves:

- `square`, `triangle`, and `polygon` use explicit point lists
- `circle` uses `rotatedRect` or `rect`
- `line` uses `startPt` and `endPt`; curved lines also use `controlPoint1` / `controlPoint2`
- dashed/dotted shapes use `appearance -> dashStyle -> pattern`
- `partialshape` represents partially erased shape/line fragments; use its `strokePath` outline as a filled path to preserve the erased gap size

Layering is reconstructed from page backgrounds first, then text, text blocks, images, and handwriting according to the parsed order and z-order where available.

Important limitations when converting:

- text layout is heuristic because exact Notability glyph shaping is not fully encoded
- raw partial erase behavior is only partly understood, but `partialshape` fragments preserve erased size for partially erased shapes/lines
- highlighter dotted/dashed strokes can differ from Notability's exported PDF because the export may use filled fragments instead of native dash arrays
- image rotation, crop, and horizontal/vertical flips are separate fields and all must be applied
