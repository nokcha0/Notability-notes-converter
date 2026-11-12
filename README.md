# Notability Notes Converter

Converts `.note` and `.ntb` files to `.pdf` by default, or optionally to `.svg`.

I originally made this for personal use because I couldn't find a converter online. I hope it is useful to others as well.

Note: output is not perfectly identical to Notability’s official export. Some details, such as Apple-specific fonts and exact text layout are approximated, but for most notes this converter works pretty well.

## How to use

### In your browser

Link: https://nokcha0.github.io/Notability-notes-converter/

Conversion happens on your browser, and your files are not sent to a server.

### From the command line

Requirement: [Rust](https://rust-lang.org/tools/install/)

SVG output with imported PDF backgrounds also needs Poppler's `pdftoppm` in `PATH`. PDF output does not need Poppler.

First, clone the repository.

Put your exported `.note` and `.ntb` files in `input/`, then run this from the repository root:

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
│       └── example2.ntb
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

A stroke can cross page boundaries. Keep its full path and width data on every page it reaches, then clip to each page. For cubic curves, use the curve's extrema rather than its control points to find the page range.

Text appears in two forms:

- `richText -> attributedString`: the main text stream
- `TextBlockMediaObject` entries in `mediaObjects`: positioned text boxes

The attributed string stores plain text in `stringKey` and style runs in `subRangesKey`. Useful style fields include:

- `NSFontSizeAttribute` and `NSFontNameAttribute`
- `subRangeColorCrossPlatformKey` or `subRangeColorKey`
- underline, strikethrough, and baseline offset in `subRangeOtherAttributesKey`
- list/checklist metadata such as `indent-level`, `indent-decoration-style`, `indent-decoration-number`, and `checklist-checked`

Style ranges count UTF-16 units. Convert them to character offsets so styles after emoji and other surrogate pairs stay aligned. Plain text boxes can share paper metadata with sticky notes; only add their paper background when a color is stored.

To convert text, reconstruct a per-character style map from the ranges, then lay out the text in document coordinates. This is the least exact part of the format because Notability's native font shaping and wrapping are not stored as final glyph positions.

Images are `mediaObjects` with nested figure/background/snapshot objects pointing to files in `Images/`. Important fields are:

- `documentOrigin`: document-space position
- `unscaledContentSize`: displayed size
- `rotationDegrees`: rotation; some files store radians despite the name
- `isFlippedHorizontal` and `isFlippedVertical`: editor mirror state, not extra flips for saved snapshots
- `FigureCropRectKey`: crop rectangle in source-image pixels
- `zIndex`: layer order

To convert images, use the referenced snapshot from `Images/` as supplied, then place it at `documentOrigin` with its stored size and rotation. Apply `FigureCropRectKey` as a clip around the source image. Keep fractional boundaries instead of rounding them to pixels. RGB JPEGs can be embedded directly, including when cropped, without being decoded and re-encoded.

Imported PDF backgrounds are described by `richText -> pageLayoutArray`. Each entry maps:

- note page index
- source PDF filename under `PDFs/`
- source page index inside that PDF

To convert a PDF-backed page, use the source PDF page as the base layer, scale it to the reconstructed output page box, then draw the note overlays above it.

Use the visible PDF crop box and inherited page rotation when finding the page ratio. A quarter-turn swaps its width and height.

Handwriting is stored under `richText -> Handwriting Overlay -> SpatialHash`. The main stroke payloads are little-endian binary arrays:

- `curvespoints`: `f32` x/y point pairs
- `curvesnumpoints`: `i32` point counts per curve
- `curveswidth`: base stroke widths as `f32`
- `curvesforces`: pressure samples as `f32`
- `curvesfractionalwidths`: width multipliers as `f32`
- `curvescolors`: raw RGBA bytes
- `curvesstyles`: tool/style ids
- `curveUUIDs`: original stroke ids; erased fragments can share an id
- `dashStyles`: dash/dot pattern metadata keyed by curve index
- `groupsArrays`: nested stroke groups with transforms
- `shapes`: simple shape strokes

Many curves are cubic Bezier chains: the point count follows `1 + 3n`, where each segment uses one start point and three more points. Pressure and fractional-width arrays may align to the Bezier sample count rather than the raw point count. A converter should preserve that pressure data for pen and pencil width/opacity.

Polygon vertices can also have a `1 + 3n` point count. Keep them as straight edges, including when drawing with pencil or variable width. Small dots and short pen marks should not be discarded just because they are thin or dark.

Dash and dot patterns live in `dashStyles -> objectPatterns -> <curve-index> -> pattern`. Keep the original curve index when attaching dash metadata; filtering or reordering curves too early can assign dotted styles to the wrong stroke.

Some files store styles and dash patterns per original stroke instead of per curve. When the style count matches the stroke count, use `curveUUIDs` to give erased fragments the same style.

Grouped handwriting in `groupsArrays` has its own nested archive data plus a transform. Apply the group transform to stroke coordinates. Scale stroke width by the transform as well, otherwise grouped strokes render too thick or too thin.

Shape strokes are separate from normal raw curves:

- `square`, `triangle`, and `polygon` use explicit point lists
- `circle` and `ellipse` use `rotatedRect` or `rect`
- `line` uses `startPt` and `endPt`; curved lines also use `controlPoint1` / `controlPoint2`
- dashed/dotted shapes use `appearance -> dashStyle -> pattern`
- `partialshape` represents partially erased shape/line fragments; use its `strokePath` outline as a filled path to preserve the erased gap size

Layering is reconstructed from page backgrounds first, then text, text blocks, images, and handwriting according to the parsed order and z-order where available.

Important limitations when converting:

- text layout is heuristic because exact Notability glyph shaping is not fully encoded
- raw partial erase behavior is only partly understood, but `partialshape` fragments preserve erased size for partially erased shapes/lines
- highlighter dotted/dashed strokes can differ from Notability's exported PDF because the export may use filled fragments instead of native dash arrays
- image snapshots already have their exported orientation; legacy editor flip flags should not be applied again. Keep the snapshot rotation and crop

## Analysis of the `.ntb` file format

A `.ntb` file is a more recent file format by Notability. It is also a ZIP archive, but its contents are different from a `.note` file. The main entries are:

- `noteBundle`: a FlatBuffers event collection containing the note data
- `assets/`: embedded media named with a SHA-512 content hash and a file extension

The `noteBundle` stores a history of events instead of one final document object graph. Each event has an id, a type, and a payload. These event types are currently understood:

- type 1: document and paper settings
- type 7: text boundary anchors
- type 8: text
- type 12: character style ranges
- type 13: paragraph and list style ranges
- type 15: ink
- type 18: shapes
- type 20: lists of related event ids
- type 22: images and sticky notes

Text and style data are separate events. Text events refer to their container by event id. Character and paragraph styles refer to text event ids and UTF-16 offsets. Resolve these references before applying styles, especially when the note has multiple text boxes. Type 7 events can provide end anchors for text ranges.

Positioned text boxes store a page, position, size, and rotation. The converter keeps their text and styles separate from the main document text. It also preserves main text that continues onto later pages.

Ink paths use compact binary data with half-precision or single-precision values. The payload also stores the page, origin, scale, rotation, width, color, tool, and line style. The scale applies to both the path coordinates and the stroke width.

Cubic path records also contain an opacity byte. Pencil opacity varies between records, so keep it with each segment when splitting erased fragments. Use the stored opacity rather than estimating it from pressure.

Shapes are separate events. Lines can have a control point, polygons use point lists, and ellipses use a bounding size. Shape events also store page, origin, rotation, width, color, tool, and dash style.

Transformed groups store rotation on each child event. Apply it to ink and shapes around each event origin. Line style 2 is dashed and line style 3 is dotted.

Partial eraser and shape eraser results are stored as breaks inside an ink path. In cubic paths, command 0 continues the curve and command 3 moves to the next fragment. In linear paths, command 6 continues the line and command 7 moves to the next fragment. A move command must start a new path. Drawing a line across it fills the erased area again.

Type 20 events contain lists of related event ids. They do not act as eraser masks, so their referenced ink and shapes should still be rendered.

Images refer to files under `assets/` by their content hash. Their events store a MIME type, logical asset size, page, position, displayed size, rotation in radians, and an optional crop rectangle. Crop values use logical asset coordinates and do not always match the pixel dimensions of the stored image.

Scale crop coordinates from the logical asset size to the actual pixel size. Keep fractional crop boundaries by clipping the original image, rather than rounding to a new bitmap. PDF images and their transparency masks use interpolation to match the reference export. SVG images keep precise positions, sizes, and clipped viewports.

For a bundle with one PDF asset under `assets/`, the converter uses all source pages in their original order as backgrounds, including pages without handwriting. PDF output preserves the source page content, crop, and rotation. SVG output embeds rendered backgrounds while keeping note strokes as SVG paths.

Multiple PDF assets and reordered source pages still need explicit page mapping. The converter reports an error for multiple PDF assets rather than choosing one. The supplied example has no PDF backgrounds or positioned text boxes, so these paths are tested with generated fixtures.

The main differences from `.note` are:

- `.note` uses an Apple keyed archive in `Session.plist`; `.ntb` uses FlatBuffers in `noteBundle`
- `.note` mostly stores the current document state; `.ntb` stores an ordered event collection with event references
- `.note` uses one continuous vertical coordinate space; `.ntb` events contain explicit page references
- `.note` media uses paths under `Images/`; `.ntb` media uses content hashes under `assets/`
- `.note` handwriting uses parallel binary arrays; `.ntb` stores a compact path inside each ink event

## Credits

Thanks to [tim4431's fork](https://github.com/tim4431/Notability-notes-converter) for the browser converter approach and additional format findings.
