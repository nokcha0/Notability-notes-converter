use super::model::{
    MediaImage, NoteDocument, OutputFormat, StickyNote, StrokeCurve, TextBlock, TextStyle,
    CURVE_SMOOTHING_TENSION, DEFAULT_CONTENT_INSET_RATIO, DEFAULT_TEXT_TOP_DOC,
    STROKE_STYLE_HIGHLIGHTER, STROKE_STYLE_NOT_EXPORTED, STROKE_STYLE_PENCIL,
};
use super::note::read_zip_entry;
use super::util::{page_count_for, parse_line_spacing_doc};
use crate::pdf::{add_form_xobject, page_box, page_id_by_index, page_render_transform};
use crate::Result;
use flate2::{write::ZlibEncoder, Compression};
use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegEncoder;
use image::{AnimationDecoder, DynamicImage, ExtendedColorType, GenericImageView};
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
#[cfg(not(target_arch = "wasm32"))]
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Cursor, Read, Seek, Write};
use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Command, Stdio};
use zip::ZipArchive;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_note_output(
    note: &NoteDocument,
    note_path: &Path,
    output_path: &Path,
    format: OutputFormat,
) -> Result<()> {
    let mut bundle = ZipArchive::new(File::open(note_path)?)?;
    let bytes = render_note(note, &mut bundle, format)?;
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, bytes)?;
    Ok(())
}

pub(crate) fn render_note<R: Read + Seek>(
    note: &NoteDocument,
    bundle: &mut ZipArchive<R>,
    format: OutputFormat,
) -> Result<Vec<u8>> {
    match format {
        OutputFormat::Pdf => render_pdf(note, bundle),
        OutputFormat::Svg => render_svg(note, bundle, None),
    }
}

fn render_pdf<R: Read + Seek>(note: &NoteDocument, bundle: &mut ZipArchive<R>) -> Result<Vec<u8>> {
    let mut output = Document::with_version("1.5");
    let mut max_id = 1u32;
    let mut loaded_pdfs: BTreeMap<String, Document> = BTreeMap::new();
    let mut image_cache = ImageXObjectCache::new();
    for relative_path in note
        .pdf_pages
        .iter()
        .map(|page| page.relative_path.clone())
        .collect::<BTreeSet<_>>()
    {
        let data = read_zip_entry(bundle, &(note.bundle_root.clone() + &relative_path))?;
        let mut doc = Document::load_mem(&data)?;
        doc.renumber_objects_with(max_id);
        max_id = doc.max_id + 1;
        for (object_id, object) in &doc.objects {
            output.objects.insert(*object_id, object.clone());
        }
        loaded_pdfs.insert(relative_path, doc);
    }
    output.max_id = max_id.saturating_sub(1);

    let fonts = add_standard_fonts(&mut output);
    let pages_id = output.new_object_id();
    let page_count = page_count_for(note);
    let mut page_ids = Vec::with_capacity(page_count);
    for page_index in 0..page_count {
        let page_id = write_page(
            &mut output,
            bundle,
            note,
            &loaded_pdfs,
            &fonts,
            &mut image_cache,
            pages_id,
            page_index,
        )?;
        page_ids.push(page_id);
    }
    output.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
            "Count" => page_ids.len() as i64,
        }),
    );
    let catalog_id = output.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    output.trailer.set("Root", catalog_id);
    if !loaded_pdfs.is_empty() {
        output.prune_objects();
        output.renumber_objects();
    }
    let mut bytes = Vec::new();
    output.save_to(&mut bytes)?;
    Ok(bytes)
}

struct PdfFonts {
    helvetica: ObjectId,
    helvetica_bold: ObjectId,
    helvetica_oblique: ObjectId,
    helvetica_bold_oblique: ObjectId,
    courier: ObjectId,
    courier_bold: ObjectId,
    courier_oblique: ObjectId,
    courier_bold_oblique: ObjectId,
    times: ObjectId,
    times_bold: ObjectId,
    times_italic: ObjectId,
    times_bold_italic: ObjectId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ImageCacheKey {
    data_hash: u64,
    data_len: usize,
    crop: Option<[u32; 6]>,
    is_jpeg: bool,
}

type ImageXObjectCache = BTreeMap<ImageCacheKey, ObjectId>;

enum PageMedia<'a> {
    Image(&'a MediaImage),
    Sticky(&'a StickyNote),
    Text(&'a TextBlock),
}

fn page_media_layers(note: &NoteDocument, page_index: usize) -> Vec<(i64, usize, PageMedia<'_>)> {
    let mut layers = Vec::new();
    for media in note
        .media_images
        .iter()
        .filter(|m| m.page_index == page_index)
    {
        layers.push((media.z_index, layers.len(), PageMedia::Image(media)));
    }
    for sticky in note
        .sticky_notes
        .iter()
        .filter(|m| m.page_index == page_index)
    {
        layers.push((sticky.z_index, layers.len(), PageMedia::Sticky(sticky)));
    }
    // Text follows a background with the same z-index, including sticky-note text.
    for block in note
        .text_blocks
        .iter()
        .filter(|m| m.page_index == page_index)
    {
        layers.push((block.z_index, layers.len(), PageMedia::Text(block)));
    }
    layers.sort_by_key(|(z, order, _)| (*z, *order));
    layers
}

impl PdfFonts {
    fn resources(&self) -> [(&'static str, ObjectId); 12] {
        [
            ("FHelv", self.helvetica),
            ("FHelvB", self.helvetica_bold),
            ("FHelvI", self.helvetica_oblique),
            ("FHelvBI", self.helvetica_bold_oblique),
            ("FCour", self.courier),
            ("FCourB", self.courier_bold),
            ("FCourI", self.courier_oblique),
            ("FCourBI", self.courier_bold_oblique),
            ("FTimes", self.times),
            ("FTimesB", self.times_bold),
            ("FTimesI", self.times_italic),
            ("FTimesBI", self.times_bold_italic),
        ]
    }
}

fn add_standard_fonts(output: &mut Document) -> PdfFonts {
    fn add(output: &mut Document, base_font: &str) -> ObjectId {
        output.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => base_font,
        })
    }
    PdfFonts {
        helvetica: add(output, "Helvetica"),
        helvetica_bold: add(output, "Helvetica-Bold"),
        helvetica_oblique: add(output, "Helvetica-Oblique"),
        helvetica_bold_oblique: add(output, "Helvetica-BoldOblique"),
        courier: add(output, "Courier"),
        courier_bold: add(output, "Courier-Bold"),
        courier_oblique: add(output, "Courier-Oblique"),
        courier_bold_oblique: add(output, "Courier-BoldOblique"),
        times: add(output, "Times-Roman"),
        times_bold: add(output, "Times-Bold"),
        times_italic: add(output, "Times-Italic"),
        times_bold_italic: add(output, "Times-BoldItalic"),
    }
}

fn write_page<R: Read + Seek>(
    output: &mut Document,
    bundle: &mut ZipArchive<R>,
    note: &NoteDocument,
    loaded_pdfs: &BTreeMap<String, Document>,
    fonts: &PdfFonts,
    image_cache: &mut ImageXObjectCache,
    pages_id: ObjectId,
    page_index: usize,
) -> Result<ObjectId> {
    let mut operations = Vec::new();
    let mut xobjects = Dictionary::new();
    let mut ext_gstates = Dictionary::new();
    let mut ext_cache: BTreeMap<(u16, bool), String> = BTreeMap::new();
    let doc_to_pt = note.export_width_pt / note.page_width_doc;
    let content_inset_doc = note.page_width_doc * DEFAULT_CONTENT_INSET_RATIO;
    let line_spacing_doc = parse_line_spacing_doc(
        note.line_style.as_deref(),
        note.page_width_doc,
        note.layout_width_pt,
    );
    let pdf_page = note
        .pdf_pages
        .iter()
        .find(|page| page.page_index == page_index);
    if let Some(pdf_page) = pdf_page {
        if let Some(base_doc) = loaded_pdfs.get(&pdf_page.relative_path) {
            let page_id = page_id_by_index(base_doc, pdf_page.source_page_index)?;
            let bbox = page_box(base_doc, page_id)?;
            let form_id = add_form_xobject(output, base_doc, page_id, bbox)?;
            xobjects.set("Base", form_id);
            let transform = page_render_transform(
                base_doc,
                page_id,
                bbox,
                note.export_width_pt,
                note.export_height_pt,
            )?;
            operations.extend([
                Operation::new("q", vec![]),
                Operation::new("cm", transform.into_iter().map(Into::into).collect()),
                Operation::new("Do", vec!["Base".into()]),
                Operation::new("Q", vec![]),
            ]);
        }
    } else {
        draw_background(
            &mut operations,
            note,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        );
    }

    draw_text(
        &mut operations,
        note,
        page_index,
        doc_to_pt,
        content_inset_doc,
        line_spacing_doc,
    );
    let media_layers = page_media_layers(note, page_index);
    for (layer_index, (_, _, layer)) in media_layers.into_iter().enumerate() {
        match layer {
            PageMedia::Image(media) => draw_pdf_image(
                &mut operations,
                &mut xobjects,
                output,
                bundle,
                note,
                media,
                image_cache,
                layer_index,
                doc_to_pt,
                content_inset_doc,
            )?,
            PageMedia::Sticky(sticky) => draw_pdf_sticky_note(
                &mut operations,
                sticky,
                doc_to_pt,
                content_inset_doc,
                note.export_height_pt,
            ),
            PageMedia::Text(block) => draw_pdf_text_block(
                &mut operations,
                note,
                block,
                doc_to_pt,
                content_inset_doc,
                line_spacing_doc,
            ),
        }
    }
    let curves: Vec<&StrokeCurve> = note
        .curves
        .iter()
        .filter(|curve| curve.page_index == page_index)
        .collect();
    draw_curves(
        &mut operations,
        output,
        &mut ext_gstates,
        &mut ext_cache,
        &curves,
        doc_to_pt,
        content_inset_doc,
        note.export_height_pt,
    );

    let mut font_resources = Dictionary::new();
    for (name, id) in fonts.resources() {
        font_resources.set(name, id);
    }
    let mut resources = dictionary! {
        "Font" => font_resources,
    };
    if !xobjects.is_empty() {
        resources.set("XObject", xobjects);
    }
    if !ext_gstates.is_empty() {
        resources.set("ExtGState", ext_gstates);
    }
    let resources_id = output.add_object(resources);
    let content_id = output.add_object(Stream::new(
        Dictionary::new(),
        Content { operations }.encode()?,
    ));
    let page_id = output.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), note.export_width_pt.into(), note.export_height_pt.into()],
    });
    Ok(page_id)
}

fn draw_pdf_image<R: Read + Seek>(
    operations: &mut Vec<Operation>,
    xobjects: &mut Dictionary,
    output: &mut Document,
    bundle: &mut ZipArchive<R>,
    note: &NoteDocument,
    media: &MediaImage,
    image_cache: &mut ImageXObjectCache,
    image_index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
) -> Result<()> {
    let name = format!("Im{image_index}");
    let image_id = add_image_xobject(output, bundle, note, media, image_cache)?;
    xobjects.set(name.clone(), image_id);
    let x = (content_inset_doc + media.x) * doc_to_pt;
    let y = media.y * doc_to_pt;
    let width = media.width * doc_to_pt;
    let height = media.height * doc_to_pt;
    let draw_y = note.export_height_pt - y - height;
    let center_x = x + width * 0.5;
    let center_y = draw_y + height * 0.5;
    let angle = -media.rotation_degrees.to_radians();
    let cos = angle.cos();
    let sin = angle.sin();
    let image_scale_x = if media.flipped_horizontal {
        -width
    } else {
        width
    };
    let image_scale_y = if media.flipped_vertical {
        -height
    } else {
        height
    };
    let image_offset_x = if media.flipped_horizontal {
        width * 0.5
    } else {
        -width * 0.5
    };
    let image_offset_y = if media.flipped_vertical {
        height * 0.5
    } else {
        -height * 0.5
    };
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new(
            "cm",
            vec![
                1.into(),
                0.into(),
                0.into(),
                1.into(),
                center_x.into(),
                center_y.into(),
            ],
        ),
        Operation::new(
            "cm",
            vec![
                cos.into(),
                sin.into(),
                (-sin).into(),
                cos.into(),
                0.into(),
                0.into(),
            ],
        ),
        Operation::new(
            "cm",
            vec![
                image_scale_x.into(),
                0.into(),
                0.into(),
                image_scale_y.into(),
                image_offset_x.into(),
                image_offset_y.into(),
            ],
        ),
        Operation::new("Do", vec![name.into()]),
        Operation::new("Q", vec![]),
    ]);
    Ok(())
}

fn draw_pdf_sticky_note(
    operations: &mut Vec<Operation>,
    sticky: &StickyNote,
    doc_to_pt: f32,
    content_inset_doc: f32,
    export_height_pt: f32,
) {
    let x = (content_inset_doc + sticky.x) * doc_to_pt;
    let y = sticky.y * doc_to_pt;
    let width = sticky.width * doc_to_pt;
    let height = sticky.height * doc_to_pt;
    let draw_y = export_height_pt - y - height;
    let center_x = x + width * 0.5;
    let center_y = draw_y + height * 0.5;
    let angle = -sticky.rotation_degrees.to_radians();
    let cos = angle.cos();
    let sin = angle.sin();
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new(
            "cm",
            vec![
                1.into(),
                0.into(),
                0.into(),
                1.into(),
                center_x.into(),
                center_y.into(),
            ],
        ),
        Operation::new(
            "cm",
            vec![
                cos.into(),
                sin.into(),
                (-sin).into(),
                cos.into(),
                0.into(),
                0.into(),
            ],
        ),
        Operation::new(
            "rg",
            vec![
                (sticky.color[0] as f32 / 255.0).into(),
                (sticky.color[1] as f32 / 255.0).into(),
                (sticky.color[2] as f32 / 255.0).into(),
            ],
        ),
        Operation::new(
            "re",
            vec![
                (-width * 0.5).into(),
                (-height * 0.5).into(),
                width.into(),
                height.into(),
            ],
        ),
        Operation::new("f", vec![]),
    ]);
    if sticky
        .line_style
        .as_deref()
        .unwrap_or("")
        .starts_with("Lines")
    {
        let spacing_pt = sticky
            .line_style
            .as_deref()
            .and_then(|style| style.split(':').next_back())
            .and_then(|value| value.parse::<f32>().ok())
            .map(|inches| inches * 72.0)
            .unwrap_or(14.0);
        operations.extend([
            Operation::new(
                "RG",
                vec![
                    (205.0 / 255.0).into(),
                    (190.0 / 255.0).into(),
                    (95.0 / 255.0).into(),
                ],
            ),
            Operation::new("w", vec![0.35.into()]),
        ]);
        let mut line_y = spacing_pt * sticky.line_start_multiple;
        while line_y < height - spacing_pt * 0.25 {
            let py = height * 0.5 - line_y;
            operations.extend([
                Operation::new("m", vec![(-width * 0.5).into(), py.into()]),
                Operation::new("l", vec![(width * 0.5).into(), py.into()]),
                Operation::new("S", vec![]),
            ]);
            line_y += spacing_pt;
        }
    }
    operations.push(Operation::new("Q", vec![]));
}

fn draw_background(
    operations: &mut Vec<Operation>,
    note: &NoteDocument,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) {
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new("rg", vec![1.into(), 1.into(), 1.into()]),
        Operation::new(
            "re",
            vec![
                0.into(),
                0.into(),
                note.export_width_pt.into(),
                note.export_height_pt.into(),
            ],
        ),
        Operation::new("f", vec![]),
        Operation::new("Q", vec![]),
    ]);
    if !note
        .line_style
        .as_deref()
        .unwrap_or("")
        .starts_with("Lines")
    {
        return;
    }
    let margin = content_inset_doc * doc_to_pt;
    let mut y = line_spacing_doc * doc_to_pt;
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new(
            "RG",
            vec![
                (163.0 / 255.0).into(),
                (183.0 / 255.0).into(),
                (211.0 / 255.0).into(),
            ],
        ),
        Operation::new("w", vec![0.5.into()]),
    ]);
    while y < note.export_height_pt {
        let py = note.export_height_pt - y;
        operations.extend([
            Operation::new("m", vec![margin.into(), py.into()]),
            Operation::new("l", vec![(note.export_width_pt - margin).into(), py.into()]),
            Operation::new("S", vec![]),
        ]);
        y += line_spacing_doc * doc_to_pt;
    }
    operations.push(Operation::new("Q", vec![]));
}

fn draw_text(
    operations: &mut Vec<Operation>,
    note: &NoteDocument,
    page_index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) {
    draw_text_content(
        operations,
        &note.text,
        &note.text_spans,
        page_index,
        note.page_height_doc,
        note.export_height_pt,
        0.0,
        DEFAULT_TEXT_TOP_DOC,
        note.page_width_doc - content_inset_doc * 2.0,
        doc_to_pt,
        content_inset_doc,
        line_spacing_doc,
    );
}

fn draw_pdf_text_block(
    operations: &mut Vec<Operation>,
    note: &NoteDocument,
    block: &TextBlock,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) {
    let page_index = block.page_index;
    let x = (block.x + content_inset_doc) * doc_to_pt;
    let y = note.export_height_pt - (block.y + block.height) * doc_to_pt;
    let width = block.width * doc_to_pt;
    let height = block.height * doc_to_pt;
    let cx = x + width * 0.5;
    let cy = y + height * 0.5;
    let (sin, cos) = (-block.rotation_degrees.to_radians()).sin_cos();
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new(
            "cm",
            vec![
                cos.into(),
                sin.into(),
                (-sin).into(),
                cos.into(),
                (cx - cos * cx + sin * cy).into(),
                (cy - sin * cx - cos * cy).into(),
            ],
        ),
        Operation::new("re", vec![x.into(), y.into(), width.into(), height.into()]),
        Operation::new("W", vec![]),
        Operation::new("n", vec![]),
    ]);
    draw_text_content(
        operations,
        &block.text,
        &block.text_spans,
        page_index,
        note.page_height_doc,
        note.export_height_pt,
        block.x,
        block.y + block.page_index as f32 * note.page_height_doc,
        block.width,
        doc_to_pt,
        content_inset_doc,
        line_spacing_doc,
    );
    operations.push(Operation::new("Q", vec![]));
}

fn draw_text_content(
    operations: &mut Vec<Operation>,
    text: &str,
    text_spans: &[super::model::TextSpan],
    target_page_index: usize,
    page_height_doc: f32,
    export_height_pt: f32,
    origin_x_doc: f32,
    origin_y_doc: f32,
    max_width_doc: f32,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) {
    let mut cursor_x_doc = 0.0f32;
    let mut cursor_y_doc = 0.0f32;
    let mut at_line_start = true;
    let mut line_height_doc = 0.0f32;
    let styles = build_style_map_for(text, text_spans);
    for (char_index, ch) in text.chars().enumerate() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            cursor_x_doc = 0.0;
            cursor_y_doc += text_line_advance_doc(line_spacing_doc, line_height_doc);
            line_height_doc = 0.0;
            at_line_start = true;
            continue;
        }
        let style = styles
            .get(char_index)
            .cloned()
            .unwrap_or_else(default_text_style);
        line_height_doc =
            line_height_doc.max(style.font_size * style.line_spacing_multiplier.max(1.0));
        if at_line_start {
            let absolute_y = origin_y_doc + cursor_y_doc;
            let marker_page = (absolute_y / page_height_doc).floor() as usize;
            if marker_page == target_page_index {
                draw_pdf_list_marker(
                    operations,
                    &style,
                    origin_x_doc,
                    absolute_y - marker_page as f32 * page_height_doc,
                    doc_to_pt,
                    content_inset_doc,
                    export_height_pt,
                );
            }
            cursor_x_doc += list_text_indent_doc(&style);
            at_line_start = false;
        }
        let font_pt = style.font_size * doc_to_pt;
        let char_width_doc = style.font_size * font_width_factor(&style, ch);
        if cursor_x_doc + char_width_doc > max_width_doc {
            cursor_x_doc = list_text_indent_doc(&style);
            cursor_y_doc += text_line_advance_doc(line_spacing_doc, line_height_doc);
            line_height_doc = style.font_size * style.line_spacing_multiplier.max(1.0);
        }
        let absolute_y_doc = origin_y_doc + cursor_y_doc;
        let page_index = (absolute_y_doc / page_height_doc).floor() as usize;
        if page_index == target_page_index {
            let x = (content_inset_doc + origin_x_doc + cursor_x_doc) * doc_to_pt;
            let baseline_offset_pt = style.baseline_offset * doc_to_pt;
            let local_y_doc = absolute_y_doc - page_index as f32 * page_height_doc;
            let y = export_height_pt - (local_y_doc * doc_to_pt + font_pt) + baseline_offset_pt;
            let font_name = pdf_font_resource(&style);
            operations.extend([
                Operation::new("BT", vec![]),
                Operation::new(
                    "rg",
                    vec![
                        (style.color[0] as f32 / 255.0).into(),
                        (style.color[1] as f32 / 255.0).into(),
                        (style.color[2] as f32 / 255.0).into(),
                    ],
                ),
                Operation::new("Tf", vec![font_name.into(), font_pt.into()]),
                Operation::new("Td", vec![x.into(), y.into()]),
                Operation::new("Tj", vec![Object::string_literal(ch.to_string())]),
                Operation::new("ET", vec![]),
            ]);
            if style.underline {
                draw_text_rule(
                    operations,
                    &style,
                    x,
                    y - font_pt * 0.12,
                    char_width_doc * doc_to_pt,
                    font_pt,
                );
            }
            if style.strikethrough || style.checklist_checked {
                draw_text_rule(
                    operations,
                    &style,
                    x,
                    y + font_pt * 0.30,
                    char_width_doc * doc_to_pt,
                    font_pt,
                );
            }
        }
        cursor_x_doc += char_width_doc;
    }
}

fn build_style_map_for(text: &str, text_spans: &[super::model::TextSpan]) -> Vec<TextStyle> {
    let mut styles = vec![default_text_style(); text.chars().count()];
    for span in text_spans {
        for index in span.start..(span.start + span.length) {
            if let Some(slot) = styles.get_mut(index) {
                *slot = span.style.clone();
            }
        }
    }
    styles
}

fn default_text_style() -> TextStyle {
    TextStyle {
        font_size: 12.0,
        font_name: "Helvetica".to_string(),
        line_spacing_multiplier: 1.0,
        color: [0, 0, 0, 255],
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
        baseline_offset: 0.0,
        indent_level: None,
        indent_decoration_style: None,
        indent_decoration_number: None,
        checklist_checked: false,
    }
}

fn text_line_advance_doc(line_spacing_doc: f32, line_height_doc: f32) -> f32 {
    if line_height_doc <= 0.0 {
        return line_spacing_doc;
    }
    let steps = (line_height_doc / line_spacing_doc.max(f32::EPSILON))
        .ceil()
        .max(1.0);
    line_spacing_doc * steps
}

pub(super) fn main_text_page_count(note: &NoteDocument) -> usize {
    let spacing = parse_line_spacing_doc(
        note.line_style.as_deref(),
        note.page_width_doc,
        note.layout_width_pt,
    );
    let max_width = note.page_width_doc * (1.0 - DEFAULT_CONTENT_INSET_RATIO * 2.0);
    let styles = build_style_map_for(&note.text, &note.text_spans);
    let (mut x, mut y, mut line_height) = (0.0f32, 0.0f32, 0.0f32);
    let mut at_line_start = true;
    let mut count = 1;
    for (index, ch) in note.text.chars().enumerate() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            x = 0.0;
            y += text_line_advance_doc(spacing, line_height);
            line_height = 0.0;
            at_line_start = true;
            continue;
        }
        let style = &styles[index];
        line_height = line_height.max(style.font_size * style.line_spacing_multiplier.max(1.0));
        if at_line_start {
            x += list_text_indent_doc(style);
            at_line_start = false;
        }
        let width = style.font_size * font_width_factor(style, ch);
        if x + width > max_width {
            x = list_text_indent_doc(style);
            y += text_line_advance_doc(spacing, line_height);
            line_height = style.font_size * style.line_spacing_multiplier.max(1.0);
        }
        count = count.max(((DEFAULT_TEXT_TOP_DOC + y) / note.page_height_doc).floor() as usize + 1);
        x += width;
    }
    count
}

fn pdf_font_resource(style: &TextStyle) -> &'static str {
    let lower = style.font_name.to_ascii_lowercase();
    let family = if lower.contains("times")
        || lower.contains("baskerville")
        || lower.contains("alnile")
        || lower.contains("serif")
    {
        "times"
    } else if lower.contains("chalkduster") {
        "courier"
    } else if lower.contains("typewriter") || lower.contains("courier") || lower.contains("mono") {
        "courier"
    } else {
        "helvetica"
    };
    let bold = style.bold
        || lower.contains("bold")
        || lower.contains("black")
        || lower.contains("demi")
        || lower.contains("semibold")
        || lower.contains("impact")
        || lower.contains("chalkduster");
    let italic = style.italic || lower.contains("italic") || lower.contains("oblique");
    match (family, bold, italic) {
        ("times", true, true) => "FTimesBI",
        ("times", true, false) => "FTimesB",
        ("times", false, true) => "FTimesI",
        ("times", false, false) => "FTimes",
        ("courier", true, true) => "FCourBI",
        ("courier", true, false) => "FCourB",
        ("courier", false, true) => "FCourI",
        ("courier", false, false) => "FCour",
        (_, true, true) => "FHelvBI",
        (_, true, false) => "FHelvB",
        (_, false, true) => "FHelvI",
        (_, false, false) => "FHelv",
    }
}

fn draw_text_rule(
    operations: &mut Vec<Operation>,
    style: &TextStyle,
    x: f32,
    y: f32,
    width: f32,
    font_pt: f32,
) {
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new(
            "RG",
            vec![
                (style.color[0] as f32 / 255.0).into(),
                (style.color[1] as f32 / 255.0).into(),
                (style.color[2] as f32 / 255.0).into(),
            ],
        ),
        Operation::new("w", vec![(font_pt / 16.0).max(0.4).into()]),
        Operation::new("m", vec![x.into(), y.into()]),
        Operation::new("l", vec![(x + width).into(), y.into()]),
        Operation::new("S", vec![]),
        Operation::new("Q", vec![]),
    ]);
}

fn draw_pdf_list_marker(
    operations: &mut Vec<Operation>,
    style: &TextStyle,
    origin_x_doc: f32,
    cursor_y_doc: f32,
    doc_to_pt: f32,
    content_inset_doc: f32,
    export_height_pt: f32,
) {
    let decoration_style = style.indent_decoration_style.unwrap_or(0);
    if decoration_style == 0 {
        return;
    }
    let level = style.indent_level.unwrap_or(0);
    let x = (content_inset_doc + origin_x_doc + list_marker_x_doc(style)) * doc_to_pt;
    let y = export_height_pt - (cursor_y_doc * doc_to_pt + style.font_size * doc_to_pt * 0.62);
    let baseline_y = export_height_pt - (cursor_y_doc * doc_to_pt + style.font_size * doc_to_pt);
    let radius = if decoration_style == 3 {
        (style.font_size * doc_to_pt * 0.32).max(2.2)
    } else {
        (style.font_size * doc_to_pt * 0.14).max(1.0)
    };
    if decoration_style == 2 {
        let marker = list_number_marker(style);
        operations.extend([
            Operation::new("BT", vec![]),
            Operation::new(
                "rg",
                vec![
                    (style.color[0] as f32 / 255.0).into(),
                    (style.color[1] as f32 / 255.0).into(),
                    (style.color[2] as f32 / 255.0).into(),
                ],
            ),
            Operation::new(
                "Tf",
                vec![
                    pdf_font_resource(style).into(),
                    (style.font_size * doc_to_pt).into(),
                ],
            ),
            Operation::new("Td", vec![x.into(), baseline_y.into()]),
            Operation::new("Tj", vec![Object::string_literal(marker)]),
            Operation::new("ET", vec![]),
        ]);
        return;
    }
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new(
            "RG",
            vec![
                (style.color[0] as f32 / 255.0).into(),
                (style.color[1] as f32 / 255.0).into(),
                (style.color[2] as f32 / 255.0).into(),
            ],
        ),
        Operation::new(
            "rg",
            vec![
                (style.color[0] as f32 / 255.0).into(),
                (style.color[1] as f32 / 255.0).into(),
                (style.color[2] as f32 / 255.0).into(),
            ],
        ),
        Operation::new("w", vec![(radius * 0.35).max(0.5).into()]),
    ]);
    draw_circle(operations, (x, y), radius);
    if decoration_style == 3 {
        operations.push(Operation::new("S", vec![]));
        if style.checklist_checked {
            let r = radius * 0.85;
            operations.extend([
                Operation::new("m", vec![(x - r).into(), y.into()]),
                Operation::new("l", vec![(x - r * 0.25).into(), (y - r * 0.65).into()]),
                Operation::new("l", vec![(x + r).into(), (y + r * 0.75).into()]),
                Operation::new("S", vec![]),
            ]);
        }
    } else if style.checklist_checked {
        operations.push(Operation::new("S", vec![]));
        let r = radius * 0.85;
        operations.extend([
            Operation::new("m", vec![(x - r).into(), y.into()]),
            Operation::new("l", vec![(x - r * 0.25).into(), (y - r * 0.65).into()]),
            Operation::new("l", vec![(x + r).into(), (y + r * 0.75).into()]),
            Operation::new("S", vec![]),
        ]);
    } else if level == 0 {
        operations.push(Operation::new("f", vec![]));
    } else {
        operations.push(Operation::new("S", vec![]));
    }
    operations.push(Operation::new("Q", vec![]));
}

fn list_marker_x_doc(style: &TextStyle) -> f32 {
    let level = style.indent_level.unwrap_or(0) as f32;
    match style.indent_decoration_style.unwrap_or(0) {
        0 => 0.0,
        3 => level * 25.0,
        _ => level * 25.0,
    }
}

fn list_text_indent_doc(style: &TextStyle) -> f32 {
    match style.indent_decoration_style.unwrap_or(0) {
        0 => 0.0,
        2 => list_marker_x_doc(style) + style.font_size * 1.7,
        3 => list_marker_x_doc(style) + style.font_size * 1.9,
        _ => list_marker_x_doc(style) + style.font_size,
    }
}

fn list_number_marker(style: &TextStyle) -> String {
    let number = style.indent_decoration_number.unwrap_or(1).max(1);
    match style.indent_level.unwrap_or(0) {
        0 => format!("{number}."),
        level => match (level - 1) % 3 {
            0 => format!("{}.", alpha_marker(number, true)),
            1 => format!("{}.", alpha_marker(number, false)),
            _ => format!("{number}."),
        },
    }
}

fn alpha_marker(number: i64, uppercase: bool) -> String {
    let mut n = number.max(1);
    let mut chars = Vec::new();
    while n > 0 {
        n -= 1;
        let base = if uppercase { b'A' } else { b'a' };
        chars.push((base + (n % 26) as u8) as char);
        n /= 26;
    }
    chars.iter().rev().collect()
}

fn font_width_factor(style: &TextStyle, ch: char) -> f32 {
    let lower = style.font_name.to_ascii_lowercase();
    if lower.contains("typewriter")
        || lower.contains("courier")
        || lower.contains("mono")
        || lower.contains("chalkduster")
    {
        0.6
    } else {
        helvetica_width_factor(ch)
    }
}

fn helvetica_width_factor(ch: char) -> f32 {
    match ch {
        ' ' => 0.278,
        '!' => 0.278,
        '"' => 0.355,
        '#' => 0.556,
        '$' => 0.556,
        '%' => 0.889,
        '&' => 0.667,
        '\'' => 0.191,
        '(' | ')' => 0.333,
        '*' => 0.389,
        '+' => 0.584,
        ',' => 0.278,
        '-' => 0.333,
        '.' => 0.278,
        '/' => 0.278,
        '0'..='9' => 0.556,
        ':' | ';' => 0.278,
        '<' | '=' | '>' => 0.584,
        '?' => 0.556,
        '@' => 1.015,
        'A' => 0.667,
        'B' => 0.667,
        'C' => 0.722,
        'D' => 0.722,
        'E' => 0.667,
        'F' => 0.611,
        'G' => 0.778,
        'H' => 0.722,
        'I' => 0.278,
        'J' => 0.5,
        'K' => 0.667,
        'L' => 0.556,
        'M' => 0.833,
        'N' => 0.722,
        'O' => 0.778,
        'P' => 0.667,
        'Q' => 0.778,
        'R' => 0.722,
        'S' => 0.667,
        'T' => 0.611,
        'U' => 0.722,
        'V' => 0.667,
        'W' => 0.944,
        'X' => 0.667,
        'Y' => 0.667,
        'Z' => 0.611,
        '[' | '\\' | ']' => 0.278,
        '^' => 0.469,
        '_' => 0.556,
        '`' => 0.333,
        'a' => 0.556,
        'b' => 0.556,
        'c' => 0.5,
        'd' => 0.556,
        'e' => 0.556,
        'f' => 0.278,
        'g' => 0.556,
        'h' => 0.556,
        'i' => 0.222,
        'j' => 0.222,
        'k' => 0.5,
        'l' => 0.222,
        'm' => 0.833,
        'n' => 0.556,
        'o' => 0.556,
        'p' => 0.556,
        'q' => 0.556,
        'r' => 0.333,
        's' => 0.5,
        't' => 0.278,
        'u' => 0.556,
        'v' => 0.5,
        'w' => 0.722,
        'x' => 0.5,
        'y' => 0.5,
        'z' => 0.5,
        '{' => 0.334,
        '|' => 0.26,
        '}' => 0.334,
        '~' => 0.584,
        _ => 0.556,
    }
}

fn add_image_xobject<R: Read + Seek>(
    output: &mut Document,
    bundle: &mut ZipArchive<R>,
    note: &NoteDocument,
    media: &super::model::MediaImage,
    image_cache: &mut ImageXObjectCache,
) -> Result<ObjectId> {
    let path = note.bundle_root.clone() + &media.relative_path;
    let data = read_zip_entry(bundle, &path)?;
    let is_jpeg = is_jpeg_path(&media.relative_path);
    let cache_key = image_cache_key(&data, media, is_jpeg);
    if let Some(image_id) = image_cache.get(&cache_key) {
        return Ok(*image_id);
    }
    if is_jpeg {
        if let Some((width, height, 3)) = jpeg_dimensions(&data) {
            let image_id = add_direct_jpeg(output, data, width, height);
            let image_id = if let Some(transform) =
                image_crop_transform(media.crop, width as u32, height as u32)
            {
                add_cropped_image_form(output, image_id, transform)?
            } else {
                image_id
            };
            image_cache.insert(cache_key, image_id);
            return Ok(image_id);
        }
    }
    let image = load_media_image(&data, media)?;
    let (width, height) = image.dimensions();
    let rgba = image.to_rgba8();
    let has_alpha = rgba.pixels().any(|pixel| pixel.0[3] < 255);
    let rgb = image.to_rgb8();
    let mut content = Vec::new();
    let filter = if is_jpeg {
        let mut encoder = JpegEncoder::new_with_quality(&mut content, 88);
        encoder.encode(rgb.as_raw(), width, height, ExtendedColorType::Rgb8)?;
        "DCTDecode"
    } else {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(rgb.as_raw())?;
        content = encoder.finish()?;
        "FlateDecode"
    };
    let mut image_dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => width as i64,
        "Height" => height as i64,
        "ColorSpace" => "DeviceRGB",
        "BitsPerComponent" => 8,
        "Filter" => filter,
        "Interpolate" => true,
    };
    if has_alpha {
        let alpha: Vec<u8> = rgba.pixels().map(|pixel| pixel.0[3]).collect();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&alpha)?;
        let mask_id = output.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => width as i64,
                "Height" => height as i64,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8,
                "Filter" => "FlateDecode",
                "Interpolate" => true,
            },
            encoder.finish()?,
        ));
        image_dict.set("SMask", mask_id);
    }
    let image_id = output.add_object(Stream::new(image_dict, content));
    let image_id = if let Some(transform) = image_crop_transform(media.crop, width, height) {
        add_cropped_image_form(output, image_id, transform)?
    } else {
        image_id
    };
    image_cache.insert(cache_key, image_id);
    Ok(image_id)
}

fn add_direct_jpeg(output: &mut Document, data: Vec<u8>, width: u16, height: u16) -> ObjectId {
    output.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => width as i64,
            "Height" => height as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
            "Interpolate" => true,
        },
        data,
    ))
}

fn add_cropped_image_form(
    output: &mut Document,
    image_id: ObjectId,
    transform: [f32; 6],
) -> Result<ObjectId> {
    let resources = dictionary! {
        "XObject" => dictionary! {
            "Image" => image_id,
        },
    };
    let content = Content {
        operations: vec![
            Operation::new("q", vec![]),
            Operation::new("re", vec![0.into(), 0.into(), 1.into(), 1.into()]),
            Operation::new("W", vec![]),
            Operation::new("n", vec![]),
            Operation::new("cm", transform.into_iter().map(Into::into).collect()),
            Operation::new("Do", vec!["Image".into()]),
            Operation::new("Q", vec![]),
        ],
    };
    Ok(output.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "FormType" => 1,
            "BBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
            "Resources" => resources,
        },
        content.encode()?,
    )))
}

fn image_crop_transform(
    crop: Option<super::model::ImageCrop>,
    image_width: u32,
    image_height: u32,
) -> Option<[f32; 6]> {
    let crop = crop?.in_pixels(image_width, image_height);
    if crop_covers_image(Some(crop), image_width, image_height) {
        return None;
    }
    let image_width = image_width as f32;
    let image_height = image_height as f32;
    let x = crop.x.clamp(0.0, image_width);
    let y = crop.y.clamp(0.0, image_height);
    let width = crop.width.min(image_width - x);
    let height = crop.height.min(image_height - y);
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some([
        image_width / width,
        0.0,
        0.0,
        image_height / height,
        -x / width,
        (y + height - image_height) / height,
    ])
}

fn image_cache_key(data: &[u8], media: &super::model::MediaImage, is_jpeg: bool) -> ImageCacheKey {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    ImageCacheKey {
        data_hash: hasher.finish(),
        data_len: data.len(),
        crop: media.crop.map(|crop| {
            [
                crop.x.to_bits(),
                crop.y.to_bits(),
                crop.width.to_bits(),
                crop.height.to_bits(),
                crop.reference_size.map_or(0, |size| size.0.to_bits()),
                crop.reference_size.map_or(0, |size| size.1.to_bits()),
            ]
        }),
        is_jpeg,
    }
}

fn is_jpeg_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg")
        })
}

fn crop_covers_image(
    crop: Option<super::model::ImageCrop>,
    image_width: u32,
    image_height: u32,
) -> bool {
    let Some(crop) = crop else {
        return true;
    };
    let crop = crop.in_pixels(image_width, image_height);
    let epsilon = f32::EPSILON * image_width.max(image_height) as f32 * 2.0;
    crop.x.abs() <= epsilon
        && crop.y.abs() <= epsilon
        && (crop.width - image_width as f32).abs() <= epsilon
        && (crop.height - image_height as f32).abs() <= epsilon
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u16, u16, u8)> {
    if data.len() < 4 || data[0] != 0xff || data[1] != 0xd8 {
        return None;
    }
    let mut cursor = 2usize;
    while cursor + 4 <= data.len() {
        while cursor < data.len() && data[cursor] == 0xff {
            cursor += 1;
        }
        if cursor >= data.len() {
            return None;
        }
        let marker = data[cursor];
        cursor += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if cursor + 2 > data.len() {
            return None;
        }
        let length = u16::from_be_bytes(data.get(cursor..cursor + 2)?.try_into().ok()?) as usize;
        if length < 2 || cursor + length > data.len() {
            return None;
        }
        if is_jpeg_sof_marker(marker) {
            if length < 7 {
                return None;
            }
            let height = u16::from_be_bytes(data.get(cursor + 3..cursor + 5)?.try_into().ok()?);
            let width = u16::from_be_bytes(data.get(cursor + 5..cursor + 7)?.try_into().ok()?);
            let components = *data.get(cursor + 7)?;
            return (width > 0 && height > 0).then_some((width, height, components));
        }
        cursor += length;
    }
    None
}

fn is_jpeg_sof_marker(marker: u8) -> bool {
    matches!(
        marker,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

fn load_media_image(data: &[u8], media: &super::model::MediaImage) -> Result<DynamicImage> {
    let image = if media.relative_path.to_ascii_lowercase().ends_with(".gif") {
        let decoder = GifDecoder::new(Cursor::new(data))?;
        let frame = decoder
            .into_frames()
            .next()
            .transpose()?
            .ok_or("GIF contains no frames")?;
        DynamicImage::ImageRgba8(frame.into_buffer())
    } else {
        image::load_from_memory(data)?
    };
    // Keep source pixels intact. Crops are applied by a fractional clip transform.
    Ok(image)
}

fn draw_curves(
    operations: &mut Vec<Operation>,
    output: &mut Document,
    ext_gstates: &mut Dictionary,
    ext_cache: &mut BTreeMap<(u16, bool), String>,
    curves: &[&StrokeCurve],
    doc_to_pt: f32,
    content_inset_doc: f32,
    page_height: f32,
) {
    for style in [STROKE_STYLE_HIGHLIGHTER, 3, STROKE_STYLE_PENCIL] {
        for curve in curves.iter().copied().filter(|curve| {
            if style == 3 {
                curve.style != STROKE_STYLE_HIGHLIGHTER
                    && curve.style != STROKE_STYLE_PENCIL
                    && curve.style != STROKE_STYLE_NOT_EXPORTED
            } else {
                curve.style == style
            }
        }) {
            if curve.fill_path {
                draw_pen_curve(
                    operations,
                    output,
                    ext_gstates,
                    ext_cache,
                    curve,
                    doc_to_pt,
                    content_inset_doc,
                    page_height,
                );
            } else if curve.style == STROKE_STYLE_PENCIL {
                draw_pencil_curve(
                    operations,
                    output,
                    ext_gstates,
                    ext_cache,
                    curve,
                    doc_to_pt,
                    content_inset_doc,
                    page_height,
                );
            } else if curve.style != STROKE_STYLE_HIGHLIGHTER
                && curve.dash_pattern.is_none()
                && curve_has_variable_width(curve)
            {
                draw_pressure_pen_curve(
                    operations,
                    output,
                    ext_gstates,
                    ext_cache,
                    curve,
                    doc_to_pt,
                    content_inset_doc,
                    page_height,
                );
            } else {
                draw_pen_curve(
                    operations,
                    output,
                    ext_gstates,
                    ext_cache,
                    curve,
                    doc_to_pt,
                    content_inset_doc,
                    page_height,
                );
            }
        }
    }
}

pub(super) fn curve_has_variable_width(curve: &StrokeCurve) -> bool {
    fn varies(values: &[f32]) -> bool {
        let Some(first) = values.first() else {
            return false;
        };
        values.iter().any(|value| (value - first).abs() > 0.03)
    }
    varies(&curve.pressures) || varies(&curve.fractional_widths)
}

fn draw_pen_curve(
    operations: &mut Vec<Operation>,
    output: &mut Document,
    ext_gstates: &mut Dictionary,
    ext_cache: &mut BTreeMap<(u16, bool), String>,
    curve: &StrokeCurve,
    doc_to_pt: f32,
    content_inset_doc: f32,
    page_height: f32,
) {
    if curve.points.is_empty() {
        return;
    }
    if curve.fill_path {
        draw_filled_curve(
            operations,
            output,
            ext_gstates,
            ext_cache,
            curve,
            doc_to_pt,
            content_inset_doc,
            page_height,
        );
        return;
    }
    if curve.style == STROKE_STYLE_HIGHLIGHTER
        && curve.dash_pattern.is_some()
        && curve_path_extent(curve) < curve.width * 0.25
    {
        return;
    }
    let points = stroke_points_pt(curve, doc_to_pt, content_inset_doc, page_height);
    let multiply = curve.style == STROKE_STYLE_HIGHLIGHTER;
    let gs = ext_gstate_name(
        output,
        ext_gstates,
        ext_cache,
        curve.rgba[3] as f32 / 255.0,
        multiply,
    );
    push_stroke_state(operations, curve, curve.width * doc_to_pt, &gs);
    if points.len() == 1 {
        let radius = (curve.width * doc_to_pt / 2.0).max(0.5);
        draw_circle(operations, points[0], radius);
        operations.push(Operation::new("f", vec![]));
    } else {
        draw_path(operations, curve, &points);
        operations.push(Operation::new("S", vec![]));
    }
    operations.push(Operation::new("Q", vec![]));
}

fn draw_filled_curve(
    operations: &mut Vec<Operation>,
    output: &mut Document,
    ext_gstates: &mut Dictionary,
    ext_cache: &mut BTreeMap<(u16, bool), String>,
    curve: &StrokeCurve,
    doc_to_pt: f32,
    content_inset_doc: f32,
    page_height: f32,
) {
    let points = stroke_points_pt(curve, doc_to_pt, content_inset_doc, page_height);
    let multiply = curve.style == STROKE_STYLE_HIGHLIGHTER;
    let gs = ext_gstate_name(
        output,
        ext_gstates,
        ext_cache,
        curve.rgba[3] as f32 / 255.0,
        multiply,
    );
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new("gs", vec![gs.into()]),
        Operation::new(
            "rg",
            vec![
                (curve.rgba[0] as f32 / 255.0).into(),
                (curve.rgba[1] as f32 / 255.0).into(),
                (curve.rgba[2] as f32 / 255.0).into(),
            ],
        ),
    ]);
    if !draw_encoded_path(operations, &curve.path_commands, &points) {
        draw_path(operations, curve, &points);
    }
    operations.extend([Operation::new("f", vec![]), Operation::new("Q", vec![])]);
}

fn draw_pressure_pen_curve(
    operations: &mut Vec<Operation>,
    output: &mut Document,
    ext_gstates: &mut Dictionary,
    ext_cache: &mut BTreeMap<(u16, bool), String>,
    curve: &StrokeCurve,
    doc_to_pt: f32,
    content_inset_doc: f32,
    page_height: f32,
) {
    if curve.points.len() < 2 {
        return;
    }
    let points = stroke_points_pt(curve, doc_to_pt, content_inset_doc, page_height);
    let gs = ext_gstate_name(
        output,
        ext_gstates,
        ext_cache,
        curve.rgba[3] as f32 / 255.0,
        false,
    );
    if !curve.preserve_vertices && is_bezier_point_count(points.len()) {
        let sample_count = ((points.len() - 1) / 3) + 1;
        let pressures = resample_values(&curve.pressures, sample_count);
        let fracs = resample_values(&curve.fractional_widths, sample_count);
        for segment_index in 0..((points.len() - 1) / 3) {
            let width = pen_width(
                curve.width * doc_to_pt,
                (pressures[segment_index] + pressures[segment_index + 1]) * 0.5,
                (fracs[segment_index] + fracs[segment_index + 1]) * 0.5,
            );
            push_pencil_stroke_state(operations, curve, width, &gs);
            let start = points[segment_index * 3];
            let c1 = points[segment_index * 3 + 1];
            let c2 = points[segment_index * 3 + 2];
            let end = points[segment_index * 3 + 3];
            operations.extend([
                Operation::new("m", vec![start.0.into(), start.1.into()]),
                Operation::new(
                    "c",
                    vec![
                        c1.0.into(),
                        c1.1.into(),
                        c2.0.into(),
                        c2.1.into(),
                        end.0.into(),
                        end.1.into(),
                    ],
                ),
                Operation::new("S", vec![]),
                Operation::new("Q", vec![]),
            ]);
        }
    } else {
        let pressures = resample_values(&curve.pressures, points.len());
        let fracs = resample_values(&curve.fractional_widths, points.len());
        for index in 0..points.len() - 1 {
            let width = pen_width(
                curve.width * doc_to_pt,
                (pressures[index] + pressures[index + 1]) * 0.5,
                (fracs[index] + fracs[index + 1]) * 0.5,
            );
            push_pencil_stroke_state(operations, curve, width, &gs);
            operations.extend([
                Operation::new("m", vec![points[index].0.into(), points[index].1.into()]),
                Operation::new(
                    "l",
                    vec![points[index + 1].0.into(), points[index + 1].1.into()],
                ),
                Operation::new("S", vec![]),
                Operation::new("Q", vec![]),
            ]);
        }
    }
}

fn draw_pencil_curve(
    operations: &mut Vec<Operation>,
    output: &mut Document,
    ext_gstates: &mut Dictionary,
    ext_cache: &mut BTreeMap<(u16, bool), String>,
    curve: &StrokeCurve,
    doc_to_pt: f32,
    content_inset_doc: f32,
    page_height: f32,
) {
    if curve.points.is_empty() {
        return;
    }
    let points = stroke_points_pt(curve, doc_to_pt, content_inset_doc, page_height);
    if points.len() == 1 {
        let pressure = curve.pressures.first().copied().unwrap_or(1.0);
        let frac = curve.fractional_widths.first().copied().unwrap_or(1.0);
        let width = pencil_width(curve.width * doc_to_pt, pressure, frac);
        let alpha = pencil_segment_alpha(curve, 0, pressure);
        let gs = ext_gstate_name(output, ext_gstates, ext_cache, alpha, false);
        operations.extend([
            Operation::new("q", vec![]),
            Operation::new("gs", vec![gs.into()]),
            Operation::new(
                "rg",
                vec![
                    (curve.rgba[0] as f32 / 255.0).into(),
                    (curve.rgba[1] as f32 / 255.0).into(),
                    (curve.rgba[2] as f32 / 255.0).into(),
                ],
            ),
        ]);
        draw_circle(operations, points[0], width * 0.5);
        operations.extend([Operation::new("f", vec![]), Operation::new("Q", vec![])]);
        return;
    }
    if !curve.preserve_vertices && is_bezier_point_count(points.len()) {
        let sample_count = ((points.len() - 1) / 3) + 1;
        let pressures = resample_values(&curve.pressures, sample_count);
        let fracs = resample_values(&curve.fractional_widths, sample_count);
        for segment_index in 0..((points.len() - 1) / 3) {
            let pressure = (pressures[segment_index] + pressures[segment_index + 1]) / 2.0;
            let frac = (fracs[segment_index] + fracs[segment_index + 1]) / 2.0;
            let width = pencil_width(curve.width * doc_to_pt, pressure, frac);
            let alpha = pencil_segment_alpha(curve, segment_index, pressure);
            let gs = ext_gstate_name(output, ext_gstates, ext_cache, alpha, false);
            push_stroke_state(operations, curve, width, &gs);
            let start = points[segment_index * 3];
            let c1 = points[segment_index * 3 + 1];
            let c2 = points[segment_index * 3 + 2];
            let end = points[segment_index * 3 + 3];
            operations.extend([
                Operation::new("m", vec![start.0.into(), start.1.into()]),
                Operation::new(
                    "c",
                    vec![
                        c1.0.into(),
                        c1.1.into(),
                        c2.0.into(),
                        c2.1.into(),
                        end.0.into(),
                        end.1.into(),
                    ],
                ),
                Operation::new("S", vec![]),
                Operation::new("Q", vec![]),
            ]);
        }
    } else {
        let pressures = resample_values(&curve.pressures, points.len());
        let fracs = resample_values(&curve.fractional_widths, points.len());
        for index in 0..points.len() - 1 {
            let pressure = (pressures[index] + pressures[index + 1]) / 2.0;
            let frac = (fracs[index] + fracs[index + 1]) / 2.0;
            let width = pencil_width(curve.width * doc_to_pt, pressure, frac);
            let alpha = pencil_segment_alpha(curve, index, pressure);
            let gs = ext_gstate_name(output, ext_gstates, ext_cache, alpha, false);
            push_stroke_state(operations, curve, width, &gs);
            operations.extend([
                Operation::new("m", vec![points[index].0.into(), points[index].1.into()]),
                Operation::new(
                    "l",
                    vec![points[index + 1].0.into(), points[index + 1].1.into()],
                ),
                Operation::new("S", vec![]),
                Operation::new("Q", vec![]),
            ]);
        }
    }
}

fn push_stroke_state(operations: &mut Vec<Operation>, curve: &StrokeCurve, width: f32, gs: &str) {
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new("gs", vec![gs.into()]),
        Operation::new(
            "RG",
            vec![
                (curve.rgba[0] as f32 / 255.0).into(),
                (curve.rgba[1] as f32 / 255.0).into(),
                (curve.rgba[2] as f32 / 255.0).into(),
            ],
        ),
        Operation::new("w", vec![width.max(0.1).into()]),
        Operation::new("J", vec![1.into()]),
        Operation::new("j", vec![1.into()]),
    ]);
    if let Some(pattern) = curve.dash_pattern {
        let (on, off) = dash_lengths(pattern, curve.style, width);
        let dash = vec![on.into(), off.into()];
        operations.push(Operation::new("d", vec![Object::Array(dash), 0.into()]));
    }
}

fn push_pencil_stroke_state(
    operations: &mut Vec<Operation>,
    curve: &StrokeCurve,
    width: f32,
    gs: &str,
) {
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new("gs", vec![gs.into()]),
        Operation::new(
            "RG",
            vec![
                (curve.rgba[0] as f32 / 255.0).into(),
                (curve.rgba[1] as f32 / 255.0).into(),
                (curve.rgba[2] as f32 / 255.0).into(),
            ],
        ),
        Operation::new("w", vec![width.max(0.1).into()]),
        Operation::new("J", vec![0.into()]),
        Operation::new("j", vec![1.into()]),
    ]);
}

fn dash_lengths(pattern: u8, style: u8, width: f32) -> (f32, f32) {
    match (pattern, style == STROKE_STYLE_HIGHLIGHTER) {
        (1, true) => ((width * 3.0).max(3.0), (width * 2.0).max(1.8)),
        (2, true) => ((width * 0.12).max(0.12), (width * 1.8).max(1.8)),
        (1, false) => ((width * 3.0).max(3.0), (width * 2.0).max(1.2)),
        (2, false) => ((width * 0.12).max(0.12), (width * 1.6).max(0.9)),
        _ => ((width * 0.12).max(0.12), (width * 1.6).max(0.9)),
    }
}

fn curve_path_extent(curve: &StrokeCurve) -> f32 {
    let Some((first_x, first_y)) = curve.points.first().copied() else {
        return 0.0;
    };
    let (mut min_x, mut max_x) = (first_x, first_x);
    let (mut min_y, mut max_y) = (first_y, first_y);
    for (x, y) in &curve.points {
        min_x = min_x.min(*x);
        max_x = max_x.max(*x);
        min_y = min_y.min(*y);
        max_y = max_y.max(*y);
    }
    (max_x - min_x).hypot(max_y - min_y)
}

fn draw_path(operations: &mut Vec<Operation>, curve: &StrokeCurve, points: &[(f32, f32)]) {
    operations.push(Operation::new(
        "m",
        vec![points[0].0.into(), points[0].1.into()],
    ));
    if curve.preserve_vertices || points.len() == 2 {
        for point in points.iter().skip(1) {
            operations.push(Operation::new("l", vec![point.0.into(), point.1.into()]));
        }
    } else if is_bezier_point_count(points.len()) {
        for segment_index in 0..((points.len() - 1) / 3) {
            let c1 = points[segment_index * 3 + 1];
            let c2 = points[segment_index * 3 + 2];
            let end = points[segment_index * 3 + 3];
            operations.push(Operation::new(
                "c",
                vec![
                    c1.0.into(),
                    c1.1.into(),
                    c2.0.into(),
                    c2.1.into(),
                    end.0.into(),
                    end.1.into(),
                ],
            ));
        }
    } else {
        for (c1, c2, end) in smoothed_cubic_segments(points) {
            operations.push(Operation::new(
                "c",
                vec![
                    c1.0.into(),
                    c1.1.into(),
                    c2.0.into(),
                    c2.1.into(),
                    end.0.into(),
                    end.1.into(),
                ],
            ));
        }
    }
}

fn draw_encoded_path(
    operations: &mut Vec<Operation>,
    commands: &[u8],
    points: &[(f32, f32)],
) -> bool {
    if commands.is_empty() {
        return false;
    }
    let mut index = 0usize;
    for command in commands {
        match *command {
            0 => {
                let Some(point) = points.get(index) else {
                    return false;
                };
                operations.push(Operation::new("m", vec![point.0.into(), point.1.into()]));
                index += 1;
            }
            1 => {
                let Some(point) = points.get(index) else {
                    return false;
                };
                operations.push(Operation::new("l", vec![point.0.into(), point.1.into()]));
                index += 1;
            }
            3 => {
                let (Some(c1), Some(c2), Some(end)) = (
                    points.get(index),
                    points.get(index + 1),
                    points.get(index + 2),
                ) else {
                    return false;
                };
                operations.push(Operation::new(
                    "c",
                    vec![
                        c1.0.into(),
                        c1.1.into(),
                        c2.0.into(),
                        c2.1.into(),
                        end.0.into(),
                        end.1.into(),
                    ],
                ));
                index += 3;
            }
            _ => return false,
        }
    }
    index == points.len()
}

pub(super) fn smoothed_cubic_segments(
    points: &[(f32, f32)],
) -> Vec<((f32, f32), (f32, f32), (f32, f32))> {
    if points.len() < 3 {
        return Vec::new();
    }
    let scale = CURVE_SMOOTHING_TENSION / 6.0;
    let mut segments = Vec::with_capacity(points.len() - 1);
    for index in 0..points.len() - 1 {
        let p0 = if index > 0 {
            points[index - 1]
        } else {
            points[index]
        };
        let p1 = points[index];
        let p2 = points[index + 1];
        let p3 = if index + 2 < points.len() {
            points[index + 2]
        } else {
            p2
        };
        let c1 = (p1.0 + (p2.0 - p0.0) * scale, p1.1 + (p2.1 - p0.1) * scale);
        let c2 = (p2.0 - (p3.0 - p1.0) * scale, p2.1 - (p3.1 - p1.1) * scale);
        segments.push((c1, c2, p2));
    }
    segments
}

fn draw_circle(operations: &mut Vec<Operation>, center: (f32, f32), radius: f32) {
    let kappa = 0.55228475 * radius;
    let (x, y) = center;
    operations.extend([
        Operation::new("m", vec![(x + radius).into(), y.into()]),
        Operation::new(
            "c",
            vec![
                (x + radius).into(),
                (y + kappa).into(),
                (x + kappa).into(),
                (y + radius).into(),
                x.into(),
                (y + radius).into(),
            ],
        ),
        Operation::new(
            "c",
            vec![
                (x - kappa).into(),
                (y + radius).into(),
                (x - radius).into(),
                (y + kappa).into(),
                (x - radius).into(),
                y.into(),
            ],
        ),
        Operation::new(
            "c",
            vec![
                (x - radius).into(),
                (y - kappa).into(),
                (x - kappa).into(),
                (y - radius).into(),
                x.into(),
                (y - radius).into(),
            ],
        ),
        Operation::new(
            "c",
            vec![
                (x + kappa).into(),
                (y - radius).into(),
                (x + radius).into(),
                (y - kappa).into(),
                (x + radius).into(),
                y.into(),
            ],
        ),
    ]);
}

fn ext_gstate_name(
    output: &mut Document,
    ext_gstates: &mut Dictionary,
    cache: &mut BTreeMap<(u16, bool), String>,
    alpha: f32,
    multiply: bool,
) -> String {
    let key = ((alpha.clamp(0.0, 1.0) * 1000.0).round() as u16, multiply);
    if let Some(name) = cache.get(&key) {
        return name.clone();
    }
    let name = format!("GS{}", cache.len());
    let mut dict = dictionary! {
        "Type" => "ExtGState",
        "CA" => alpha.clamp(0.0, 1.0),
        "ca" => alpha.clamp(0.0, 1.0),
    };
    if multiply {
        dict.set("BM", "Multiply");
    }
    let id = output.add_object(dict);
    ext_gstates.set(name.clone(), id);
    cache.insert(key, name.clone());
    name
}

fn stroke_points_pt(
    curve: &StrokeCurve,
    doc_to_pt: f32,
    content_inset_doc: f32,
    page_height: f32,
) -> Vec<(f32, f32)> {
    curve
        .points
        .iter()
        .map(|(x, y)| {
            (
                (x + content_inset_doc) * doc_to_pt,
                page_height - y * doc_to_pt,
            )
        })
        .collect()
}

pub(super) fn pencil_width(base_width: f32, pressure: f32, frac: f32) -> f32 {
    let pressure = pressure.clamp(0.05, 2.5);
    let frac = frac.clamp(0.15, 3.0);
    (base_width * frac * (0.38 + 0.30 * pressure.sqrt())).max(0.1)
}

fn pencil_alpha(raw_alpha: u8, pressure: f32) -> f32 {
    let pressure = pressure.clamp(0.05, 2.5);
    (raw_alpha as f32 / 255.0) * (0.08 + 0.18 * pressure).clamp(0.08, 0.55)
}

fn pencil_segment_alpha(curve: &StrokeCurve, index: usize, pressure: f32) -> f32 {
    curve.opacities.get(index).map_or_else(
        || pencil_alpha(curve.rgba[3], pressure),
        |opacity| curve.rgba[3] as f32 / 255.0 * opacity.clamp(0.0, 1.0),
    )
}

pub(super) fn pen_width(base_width: f32, pressure: f32, frac: f32) -> f32 {
    let pressure = pressure.clamp(0.2, 2.5);
    let frac = frac.clamp(0.2, 3.0);
    (base_width * frac * pressure.sqrt()).max(0.1)
}

fn resample_values(values: &[f32], target_count: usize) -> Vec<f32> {
    if target_count == 0 {
        return Vec::new();
    }
    if values.is_empty() {
        return vec![1.0; target_count];
    }
    if values.len() == target_count {
        return values.to_vec();
    }
    if values.len() == 1 {
        return vec![values[0]; target_count];
    }
    if target_count == 1 {
        return vec![values[0]];
    }
    let last_source = values.len() - 1;
    let last_target = target_count - 1;
    (0..target_count)
        .map(|target| {
            let source_pos = target as f32 * last_source as f32 / last_target as f32;
            let lower = source_pos.floor() as usize;
            let upper = (lower + 1).min(last_source);
            let fraction = source_pos - lower as f32;
            values[lower] * (1.0 - fraction) + values[upper] * fraction
        })
        .collect()
}

fn is_bezier_point_count(point_count: usize) -> bool {
    point_count >= 4 && (point_count - 1) % 3 == 0
}

pub(crate) fn render_svg_with_backgrounds<R: Read + Seek>(
    note: &NoteDocument,
    bundle: &mut ZipArchive<R>,
    backgrounds: &[Vec<u8>],
) -> Result<Vec<u8>> {
    render_svg(note, bundle, Some(backgrounds))
}

fn render_svg<R: Read + Seek>(
    note: &NoteDocument,
    bundle: &mut ZipArchive<R>,
    backgrounds: Option<&[Vec<u8>]>,
) -> Result<Vec<u8>> {
    let page_count = page_count_for(note);
    let total_height = note.export_height_pt * page_count as f32;
    let doc_to_pt = note.export_width_pt / note.page_width_doc;
    let content_inset_doc = note.page_width_doc * DEFAULT_CONTENT_INSET_RATIO;
    let line_spacing_doc = parse_line_spacing_doc(
        note.line_style.as_deref(),
        note.page_width_doc,
        note.layout_width_pt,
    );
    let mut svg = String::new();
    let mut pdf_assets = BTreeMap::new();
    for page in &note.pdf_pages {
        if backgrounds.is_none() && !pdf_assets.contains_key(&page.relative_path) {
            let data = read_zip_entry(bundle, &(note.bundle_root.clone() + &page.relative_path))?;
            pdf_assets.insert(page.relative_path.clone(), data);
        }
    }
    write!(
        svg,
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{:.2}\" height=\"{:.2}\" ",
            "viewBox=\"0 0 {:.2} {:.2}\">"
        ),
        note.export_width_pt, total_height, note.export_width_pt, total_height
    )?;
    for page_index in 0..page_count {
        let page_origin_y = page_index as f32 * note.export_height_pt;
        write!(
            svg,
            concat!(
                "<defs><clipPath id=\"page-clip-{}\"><rect width=\"{:.2}\" height=\"{:.2}\"/></clipPath></defs>",
                "<g id=\"page-{}\" transform=\"translate(0,{:.2})\" clip-path=\"url(#page-clip-{})\">"
            ),
            page_index + 1,
            note.export_width_pt,
            note.export_height_pt,
            page_index + 1,
            page_origin_y,
            page_index + 1,
        )?;
        draw_svg_background(
            &mut svg,
            note,
            page_index,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        )?;
        if let Some((index, page)) = note
            .pdf_pages
            .iter()
            .enumerate()
            .find(|(_, page)| page.page_index == page_index)
        {
            if let Some(backgrounds) = backgrounds {
                let png = backgrounds.get(index).ok_or("Missing SVG PDF background")?;
                embed_svg_pdf_background(
                    &mut svg,
                    png,
                    note.export_width_pt,
                    note.export_height_pt,
                )?;
            } else {
                draw_svg_pdf_background(
                    &mut svg,
                    &pdf_assets[&page.relative_path],
                    page.source_page_index,
                    note.export_width_pt,
                    note.export_height_pt,
                )?;
            }
        }
        draw_svg_text(
            &mut svg,
            note,
            page_index,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        )?;
        draw_svg_images(
            &mut svg,
            bundle,
            note,
            page_index,
            doc_to_pt,
            content_inset_doc,
        )?;
        draw_svg_curves(
            &mut svg,
            note,
            page_index,
            doc_to_pt,
            content_inset_doc,
            note.export_height_pt,
        )?;
        svg.push_str("</g>");
    }
    svg.push_str("</svg>");
    Ok(svg.into_bytes())
}

#[cfg(not(target_arch = "wasm32"))]
fn draw_svg_pdf_background(
    svg: &mut String,
    pdf: &[u8],
    page_index: usize,
    width: f32,
    height: f32,
) -> Result<()> {
    // SVG cannot embed a PDF page as an image. Rasterize only its background;
    // handwritten paths and note annotations remain SVG elements.
    let page = (page_index + 1).to_string();
    let mut child = Command::new("pdftoppm")
        .args([
            "-f",
            &page,
            "-l",
            &page,
            "-r",
            "144",
            "-cropbox",
            "-png",
            "-singlefile",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("SVG PDF backgrounds require Poppler's pdftoppm: {error}"))?;
    let write_result = child
        .stdin
        .take()
        .ok_or("Could not open PDF renderer input")?
        .write_all(pdf);
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.into());
    }
    let output = child.wait_with_output()?;
    if !output.status.success() || !output.stdout.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(format!(
            "Could not render PDF background page {}: {}",
            page_index + 1,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    embed_svg_pdf_background(svg, &output.stdout, width, height)
}

#[cfg(target_arch = "wasm32")]
fn draw_svg_pdf_background(_: &mut String, _: &[u8], _: usize, _: f32, _: f32) -> Result<()> {
    Err("Supply rendered PDF backgrounds for SVG conversion in the browser".into())
}

fn embed_svg_pdf_background(svg: &mut String, png: &[u8], width: f32, height: f32) -> Result<()> {
    if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("Invalid SVG PDF background: expected PNG".into());
    }
    write!(svg,
        "<image x=\"0\" y=\"0\" width=\"{width:.2}\" height=\"{height:.2}\" href=\"data:image/png;base64,{}\" preserveAspectRatio=\"none\"/>",
        base64_encode(png))?;
    Ok(())
}

fn draw_svg_background(
    svg: &mut String,
    note: &NoteDocument,
    page_index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) -> Result<()> {
    write!(
        svg,
        "<rect x=\"0\" y=\"0\" width=\"{:.2}\" height=\"{:.2}\" fill=\"#ffffff\"/>",
        note.export_width_pt, note.export_height_pt
    )?;
    if note
        .pdf_pages
        .iter()
        .any(|page| page.page_index == page_index)
    {
        return Ok(());
    }
    if !note
        .line_style
        .as_deref()
        .unwrap_or("")
        .starts_with("Lines")
    {
        return Ok(());
    }
    let margin = content_inset_doc * doc_to_pt;
    let mut y = line_spacing_doc * doc_to_pt;
    while y < note.export_height_pt {
        write!(
            svg,
            concat!(
                "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" ",
                "stroke=\"#a3b7d3\" stroke-width=\"0.5\"/>"
            ),
            margin,
            y,
            note.export_width_pt - margin,
            y
        )?;
        y += line_spacing_doc * doc_to_pt;
    }
    Ok(())
}

fn draw_svg_text(
    svg: &mut String,
    note: &NoteDocument,
    page_index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) -> Result<()> {
    draw_svg_text_content(
        svg,
        note,
        page_index,
        &note.text,
        &note.text_spans,
        0.0,
        DEFAULT_TEXT_TOP_DOC,
        note.page_width_doc - content_inset_doc * 2.0,
        doc_to_pt,
        content_inset_doc,
        line_spacing_doc,
    )
}

fn draw_svg_text_block(
    svg: &mut String,
    note: &NoteDocument,
    block: &TextBlock,
    index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) -> Result<()> {
    let page_index = block.page_index;
    let x = (content_inset_doc + block.x) * doc_to_pt;
    let y = block.y * doc_to_pt;
    let width = block.width * doc_to_pt;
    let height = block.height * doc_to_pt;
    write!(svg, concat!(
            "<g transform=\"rotate({:.4} {:.2} {:.2})\"><defs>",
            "<clipPath id=\"text-clip-{}-{}\"><rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\"/>",
            "</clipPath></defs><g clip-path=\"url(#text-clip-{}-{})\">"
        ), block.rotation_degrees, x + width * 0.5, y + height * 0.5,
            page_index, index, x, y, width, height, page_index, index)?;
    draw_svg_text_content(
        svg,
        note,
        page_index,
        &block.text,
        &block.text_spans,
        block.x,
        block.y + block.page_index as f32 * note.page_height_doc,
        block.width,
        doc_to_pt,
        content_inset_doc,
        line_spacing_doc,
    )?;
    svg.push_str("</g></g>");
    Ok(())
}

fn draw_svg_text_content(
    svg: &mut String,
    note: &NoteDocument,
    page_index: usize,
    text: &str,
    text_spans: &[super::model::TextSpan],
    origin_x_doc: f32,
    origin_y_doc: f32,
    max_width_doc: f32,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) -> Result<()> {
    let mut cursor_x_doc = 0.0f32;
    let mut cursor_y_doc = 0.0f32;
    let mut at_line_start = true;
    let mut line_height_doc = 0.0f32;
    let styles = build_style_map_for(text, text_spans);
    for (char_index, ch) in text.chars().enumerate() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            cursor_x_doc = 0.0;
            cursor_y_doc += text_line_advance_doc(line_spacing_doc, line_height_doc);
            line_height_doc = 0.0;
            at_line_start = true;
            continue;
        }
        let style = styles
            .get(char_index)
            .cloned()
            .unwrap_or_else(default_text_style);
        line_height_doc =
            line_height_doc.max(style.font_size * style.line_spacing_multiplier.max(1.0));
        if at_line_start {
            draw_svg_list_marker(
                svg,
                note,
                page_index,
                &style,
                origin_x_doc,
                origin_y_doc + cursor_y_doc,
                doc_to_pt,
                content_inset_doc,
            )?;
            cursor_x_doc += list_text_indent_doc(&style);
            at_line_start = false;
        }
        let font_pt = style.font_size * doc_to_pt;
        let char_width_doc = style.font_size * font_width_factor(&style, ch);
        if cursor_x_doc + char_width_doc > max_width_doc {
            cursor_x_doc = list_text_indent_doc(&style);
            cursor_y_doc += text_line_advance_doc(line_spacing_doc, line_height_doc);
            line_height_doc = style.font_size * style.line_spacing_multiplier.max(1.0);
        }
        let absolute_y_doc = origin_y_doc + cursor_y_doc;
        let char_page_index = (absolute_y_doc / note.page_height_doc).floor() as usize;
        if char_page_index == page_index {
            let x = (content_inset_doc + origin_x_doc + cursor_x_doc) * doc_to_pt;
            let local_y_doc = absolute_y_doc - char_page_index as f32 * note.page_height_doc;
            let y = local_y_doc * doc_to_pt + font_pt - style.baseline_offset * doc_to_pt;
            write!(
                svg,
                concat!(
                    "<text x=\"{:.2}\" y=\"{:.2}\" font-family=\"{}\" font-size=\"{:.2}\" ",
                    "font-weight=\"{}\" font-style=\"{}\" text-decoration=\"{}\" ",
                    "fill=\"{}\" fill-opacity=\"{:.3}\" xml:space=\"preserve\">{}</text>"
                ),
                x,
                y,
                svg_font_family(&style),
                font_pt,
                if style.bold { "bold" } else { "normal" },
                if style.italic { "italic" } else { "normal" },
                svg_text_decoration(&style),
                svg_rgb(style.color),
                style.color[3] as f32 / 255.0,
                escape_xml_text_char(ch)
            )?;
        }
        cursor_x_doc += char_width_doc;
    }
    Ok(())
}

fn draw_svg_list_marker(
    svg: &mut String,
    note: &NoteDocument,
    page_index: usize,
    style: &TextStyle,
    origin_x_doc: f32,
    cursor_y_doc: f32,
    doc_to_pt: f32,
    content_inset_doc: f32,
) -> Result<()> {
    let decoration_style = style.indent_decoration_style.unwrap_or(0);
    if decoration_style == 0 {
        return Ok(());
    }
    let marker_page_index = (cursor_y_doc / note.page_height_doc).floor() as usize;
    if marker_page_index != page_index {
        return Ok(());
    }
    let level = style.indent_level.unwrap_or(0);
    let x = (content_inset_doc + origin_x_doc + list_marker_x_doc(style)) * doc_to_pt;
    let y = (cursor_y_doc - marker_page_index as f32 * note.page_height_doc) * doc_to_pt
        + style.font_size * doc_to_pt * 0.62;
    let baseline_y = (cursor_y_doc - marker_page_index as f32 * note.page_height_doc) * doc_to_pt
        + style.font_size * doc_to_pt;
    let radius = if decoration_style == 3 {
        (style.font_size * doc_to_pt * 0.32).max(2.2)
    } else {
        (style.font_size * doc_to_pt * 0.14).max(1.0)
    };
    if decoration_style == 2 {
        write!(
            svg,
            concat!(
                "<text x=\"{:.2}\" y=\"{:.2}\" font-family=\"{}\" font-size=\"{:.2}\" ",
                "font-weight=\"{}\" font-style=\"{}\" fill=\"{}\" xml:space=\"preserve\">{}</text>"
            ),
            x,
            baseline_y,
            svg_font_family(style),
            style.font_size * doc_to_pt,
            if style.bold { "bold" } else { "normal" },
            if style.italic { "italic" } else { "normal" },
            svg_rgb(style.color),
            escape_xml_text(&list_number_marker(style))
        )?;
    } else if decoration_style == 3 {
        write!(
            svg,
            "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
            x,
            y,
            radius,
            svg_rgb(style.color),
            (radius * 0.35).max(0.5)
        )?;
        if style.checklist_checked {
            write!(
                svg,
                "<path d=\"M {:.2} {:.2} L {:.2} {:.2} L {:.2} {:.2}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{:.2}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>",
                x - radius * 0.85,
                y,
                x - radius * 0.20,
                y + radius * 0.60,
                x + radius * 0.90,
                y - radius * 0.75,
                svg_rgb(style.color),
                (radius * 0.35).max(0.5)
            )?;
        }
    } else if style.checklist_checked {
        write!(
            svg,
            concat!(
                "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
                "<path d=\"M {:.2} {:.2} L {:.2} {:.2} L {:.2} {:.2}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{:.2}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>"
            ),
            x,
            y,
            radius,
            svg_rgb(style.color),
            (radius * 0.35).max(0.5),
            x - radius * 0.85,
            y,
            x - radius * 0.20,
            y + radius * 0.60,
            x + radius * 0.90,
            y - radius * 0.75,
            svg_rgb(style.color),
            (radius * 0.35).max(0.5)
        )?;
    } else if level == 0 {
        write!(
            svg,
            "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"{}\"/>",
            x,
            y,
            radius,
            svg_rgb(style.color)
        )?;
    } else {
        write!(
            svg,
            "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{:.2}\"/>",
            x,
            y,
            radius,
            svg_rgb(style.color),
            (radius * 0.35).max(0.5)
        )?;
    }
    Ok(())
}

fn draw_svg_images<R: Read + Seek>(
    svg: &mut String,
    bundle: &mut ZipArchive<R>,
    note: &NoteDocument,
    page_index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
) -> Result<()> {
    let media_layers = page_media_layers(note, page_index);
    for (index, (_, _, layer)) in media_layers.into_iter().enumerate() {
        match layer {
            PageMedia::Image(media) => {
                draw_svg_image(svg, bundle, note, media, doc_to_pt, content_inset_doc)?
            }
            PageMedia::Sticky(sticky) => {
                draw_svg_sticky_note(svg, sticky, doc_to_pt, content_inset_doc)?
            }
            PageMedia::Text(block) => {
                let spacing = parse_line_spacing_doc(
                    note.line_style.as_deref(),
                    note.page_width_doc,
                    note.layout_width_pt,
                );
                draw_svg_text_block(
                    svg,
                    note,
                    block,
                    index,
                    doc_to_pt,
                    content_inset_doc,
                    spacing,
                )?;
            }
        }
    }
    Ok(())
}

fn draw_svg_image<R: Read + Seek>(
    svg: &mut String,
    bundle: &mut ZipArchive<R>,
    note: &NoteDocument,
    media: &MediaImage,
    doc_to_pt: f32,
    content_inset_doc: f32,
) -> Result<()> {
    let path = note.bundle_root.clone() + &media.relative_path;
    let data = read_zip_entry(bundle, &path)?;
    let mime_type = media_mime_type(&media.relative_path);
    let crop_transform = if media.crop.is_some() {
        let (width, height) = if let Some((width, height, _)) = jpeg_dimensions(&data) {
            (width as u32, height as u32)
        } else {
            load_media_image(&data, media)?.dimensions()
        };
        image_crop_transform(media.crop, width, height)
    } else {
        None
    };
    let x = (content_inset_doc + media.x) * doc_to_pt;
    let y = media.y * doc_to_pt;
    let width = media.width * doc_to_pt;
    let height = media.height * doc_to_pt;
    let center_x = x + width * 0.5;
    let center_y = y + height * 0.5;
    let scale_x = if media.flipped_horizontal { -1.0 } else { 1.0 };
    let scale_y = if media.flipped_vertical { -1.0 } else { 1.0 };
    let transform = if media.rotation_degrees.abs() > f32::EPSILON
        || media.flipped_horizontal
        || media.flipped_vertical
    {
        format!(
                " transform=\"translate({:.5} {:.5}) rotate({:.6}) scale({:.1} {:.1}) translate({:.5} {:.5})\"",
                center_x,
                center_y,
                media.rotation_degrees,
                scale_x,
                scale_y,
                -center_x,
                -center_y
            )
    } else {
        String::new()
    };
    if let Some([sx, _, _, sy, tx, ty]) = crop_transform {
        write!(
            svg,
            concat!(
                "<g{}><svg x=\"{:.5}\" y=\"{:.5}\" width=\"{:.5}\" height=\"{:.5}\" ",
                "viewBox=\"0 0 1 1\" preserveAspectRatio=\"none\" overflow=\"hidden\">",
                "<image x=\"{:.8}\" y=\"{:.8}\" width=\"{:.8}\" height=\"{:.8}\" ",
                "href=\"data:{};base64,{}\" preserveAspectRatio=\"none\"/></svg></g>"
            ),
            transform,
            x,
            y,
            width,
            height,
            tx,
            1.0 - sy - ty,
            sx,
            sy,
            mime_type,
            base64_encode(&data)
        )?;
        return Ok(());
    }
    write!(
        svg,
        concat!(
            "<image x=\"{:.5}\" y=\"{:.5}\" width=\"{:.5}\" height=\"{:.5}\" ",
            "href=\"data:{};base64,{}\" preserveAspectRatio=\"none\"{}/>"
        ),
        x,
        y,
        width,
        height,
        mime_type,
        base64_encode(&data),
        transform
    )?;
    Ok(())
}

fn draw_svg_sticky_note(
    svg: &mut String,
    sticky: &StickyNote,
    doc_to_pt: f32,
    content_inset_doc: f32,
) -> Result<()> {
    let x = (content_inset_doc + sticky.x) * doc_to_pt;
    let y = sticky.y * doc_to_pt;
    let width = sticky.width * doc_to_pt;
    let height = sticky.height * doc_to_pt;
    let center_x = x + width * 0.5;
    let center_y = y + height * 0.5;
    let transform = if sticky.rotation_degrees.abs() > f32::EPSILON {
        format!(
            " transform=\"rotate({:.4} {:.2} {:.2})\"",
            sticky.rotation_degrees, center_x, center_y
        )
    } else {
        String::new()
    };
    write!(
        svg,
        "<g{}><rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"{}\" fill-opacity=\"{:.3}\"/>",
        transform,
        x,
        y,
        width,
        height,
        svg_rgb(sticky.color),
        sticky.color[3] as f32 / 255.0
    )?;
    if sticky
        .line_style
        .as_deref()
        .unwrap_or("")
        .starts_with("Lines")
    {
        let spacing = sticky
            .line_style
            .as_deref()
            .and_then(|style| style.split(':').next_back())
            .and_then(|value| value.parse::<f32>().ok())
            .map(|inches| inches * 72.0)
            .unwrap_or(14.0);
        let mut line_y = spacing * sticky.line_start_multiple;
        while line_y < height - spacing * 0.25 {
            write!(
                svg,
                "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"#cdbe5f\" stroke-width=\"0.35\"/>",
                x,
                y + line_y,
                x + width,
                y + line_y
            )?;
            line_y += spacing;
        }
    }
    svg.push_str("</g>");
    Ok(())
}

fn draw_svg_curves(
    svg: &mut String,
    note: &NoteDocument,
    page_index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
    page_height: f32,
) -> Result<()> {
    let curves: Vec<&StrokeCurve> = note
        .curves
        .iter()
        .filter(|curve| curve.page_index == page_index)
        .collect();
    for style in [STROKE_STYLE_HIGHLIGHTER, 3, STROKE_STYLE_PENCIL] {
        for curve in curves.iter().copied().filter(|curve| {
            if style == 3 {
                curve.style != STROKE_STYLE_HIGHLIGHTER
                    && curve.style != STROKE_STYLE_PENCIL
                    && curve.style != STROKE_STYLE_NOT_EXPORTED
            } else {
                curve.style == style
            }
        }) {
            if curve.fill_path {
                draw_svg_pen_curve(svg, curve, doc_to_pt, content_inset_doc, page_height)?;
            } else if curve.style == STROKE_STYLE_PENCIL {
                draw_svg_pencil_curve(svg, curve, doc_to_pt, content_inset_doc)?;
            } else if curve.style != STROKE_STYLE_HIGHLIGHTER
                && curve.dash_pattern.is_none()
                && curve_has_variable_width(curve)
            {
                draw_svg_pressure_pen_curve(svg, curve, doc_to_pt, content_inset_doc)?;
            } else {
                draw_svg_pen_curve(svg, curve, doc_to_pt, content_inset_doc, page_height)?;
            }
        }
    }
    Ok(())
}

fn draw_svg_pen_curve(
    svg: &mut String,
    curve: &StrokeCurve,
    doc_to_pt: f32,
    content_inset_doc: f32,
    _page_height: f32,
) -> Result<()> {
    if curve.points.is_empty() {
        return Ok(());
    }
    if curve.style == STROKE_STYLE_HIGHLIGHTER
        && curve.dash_pattern.is_some()
        && curve_path_extent(curve) < curve.width * 0.25
    {
        return Ok(());
    }
    let points = stroke_points_svg(curve, doc_to_pt, content_inset_doc);
    let alpha = curve.rgba[3] as f32 / 255.0;
    let multiply = curve.style == STROKE_STYLE_HIGHLIGHTER;
    if curve.fill_path {
        write!(
            svg,
            "<path d=\"{}\" fill=\"{}\" fill-opacity=\"{:.3}\"{} />",
            svg_path_data(curve, &points),
            svg_rgb(curve.rgba),
            alpha,
            svg_blend_attr(multiply)
        )?;
        return Ok(());
    }
    if points.len() == 1 {
        let radius = (curve.width * doc_to_pt / 2.0).max(0.5);
        write!(
            svg,
            concat!(
                "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"{}\" fill-opacity=\"{:.3}\"{} />"
            ),
            points[0].0,
            points[0].1,
            radius,
            svg_rgb(curve.rgba),
            alpha,
            svg_blend_attr(multiply)
        )?;
    } else {
        write!(
            svg,
            concat!(
                "<path d=\"{}\" fill=\"none\" stroke=\"{}\" stroke-opacity=\"{:.3}\" ",
                "stroke-width=\"{:.2}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"{}{} />"
            ),
            svg_path_data(curve, &points),
            svg_rgb(curve.rgba),
            alpha,
            (curve.width * doc_to_pt).max(0.1),
            svg_dash_attr(curve, doc_to_pt),
            svg_blend_attr(multiply)
        )?;
    }
    Ok(())
}

fn draw_svg_pencil_curve(
    svg: &mut String,
    curve: &StrokeCurve,
    doc_to_pt: f32,
    content_inset_doc: f32,
) -> Result<()> {
    if curve.points.is_empty() {
        return Ok(());
    }
    let points = stroke_points_svg(curve, doc_to_pt, content_inset_doc);
    if points.len() == 1 {
        let pressure = curve.pressures.first().copied().unwrap_or(1.0);
        let frac = curve.fractional_widths.first().copied().unwrap_or(1.0);
        let width = pencil_width(curve.width * doc_to_pt, pressure, frac);
        let alpha = pencil_segment_alpha(curve, 0, pressure);
        write!(
            svg,
            "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"{}\" fill-opacity=\"{:.3}\" />",
            points[0].0,
            points[0].1,
            width * 0.5,
            svg_rgb(curve.rgba),
            alpha
        )?;
        return Ok(());
    }
    if !curve.preserve_vertices && is_bezier_point_count(points.len()) {
        let sample_count = ((points.len() - 1) / 3) + 1;
        let pressures = resample_values(&curve.pressures, sample_count);
        let fracs = resample_values(&curve.fractional_widths, sample_count);
        for segment_index in 0..((points.len() - 1) / 3) {
            let pressure = (pressures[segment_index] + pressures[segment_index + 1]) / 2.0;
            let frac = (fracs[segment_index] + fracs[segment_index + 1]) / 2.0;
            let width = pencil_width(curve.width * doc_to_pt, pressure, frac);
            let alpha = pencil_segment_alpha(curve, segment_index, pressure);
            let start = points[segment_index * 3];
            let c1 = points[segment_index * 3 + 1];
            let c2 = points[segment_index * 3 + 2];
            let end = points[segment_index * 3 + 3];
            write!(
                svg,
                concat!(
                    "<path d=\"M {:.2} {:.2} C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2}\" ",
                    "fill=\"none\" stroke=\"{}\" stroke-opacity=\"{:.3}\" stroke-width=\"{:.2}\" ",
                    "stroke-linecap=\"butt\" stroke-linejoin=\"round\" />"
                ),
                start.0,
                start.1,
                c1.0,
                c1.1,
                c2.0,
                c2.1,
                end.0,
                end.1,
                svg_rgb(curve.rgba),
                alpha,
                width.max(0.1)
            )?;
        }
    } else {
        let pressures = resample_values(&curve.pressures, points.len());
        let fracs = resample_values(&curve.fractional_widths, points.len());
        for index in 0..points.len() - 1 {
            let pressure = (pressures[index] + pressures[index + 1]) / 2.0;
            let frac = (fracs[index] + fracs[index + 1]) / 2.0;
            let width = pencil_width(curve.width * doc_to_pt, pressure, frac);
            let alpha = pencil_segment_alpha(curve, index, pressure);
            write!(
                svg,
                concat!(
                    "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"{}\" ",
                    "stroke-opacity=\"{:.3}\" stroke-width=\"{:.2}\" stroke-linecap=\"butt\" />"
                ),
                points[index].0,
                points[index].1,
                points[index + 1].0,
                points[index + 1].1,
                svg_rgb(curve.rgba),
                alpha,
                width.max(0.1)
            )?;
        }
    }
    Ok(())
}

fn draw_svg_pressure_pen_curve(
    svg: &mut String,
    curve: &StrokeCurve,
    doc_to_pt: f32,
    content_inset_doc: f32,
) -> Result<()> {
    if curve.points.len() < 2 {
        return draw_svg_pen_curve(svg, curve, doc_to_pt, content_inset_doc, f32::MAX);
    }
    let points = stroke_points_svg(curve, doc_to_pt, content_inset_doc);
    let alpha = curve.rgba[3] as f32 / 255.0;
    if !curve.preserve_vertices && is_bezier_point_count(points.len()) {
        let sample_count = ((points.len() - 1) / 3) + 1;
        let pressures = resample_values(&curve.pressures, sample_count);
        let fracs = resample_values(&curve.fractional_widths, sample_count);
        for segment_index in 0..((points.len() - 1) / 3) {
            let width = pen_width(
                curve.width * doc_to_pt,
                (pressures[segment_index] + pressures[segment_index + 1]) * 0.5,
                (fracs[segment_index] + fracs[segment_index + 1]) * 0.5,
            );
            let start = points[segment_index * 3];
            let c1 = points[segment_index * 3 + 1];
            let c2 = points[segment_index * 3 + 2];
            let end = points[segment_index * 3 + 3];
            write!(
                svg,
                concat!(
                    "<path d=\"M {:.2} {:.2} C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2}\" ",
                    "fill=\"none\" stroke=\"{}\" stroke-opacity=\"{:.3}\" stroke-width=\"{:.2}\" ",
                    "stroke-linecap=\"butt\" stroke-linejoin=\"round\" />"
                ),
                start.0,
                start.1,
                c1.0,
                c1.1,
                c2.0,
                c2.1,
                end.0,
                end.1,
                svg_rgb(curve.rgba),
                alpha,
                width
            )?;
        }
    } else {
        let pressures = resample_values(&curve.pressures, points.len());
        let fracs = resample_values(&curve.fractional_widths, points.len());
        for index in 0..points.len() - 1 {
            let width = pen_width(
                curve.width * doc_to_pt,
                (pressures[index] + pressures[index + 1]) * 0.5,
                (fracs[index] + fracs[index + 1]) * 0.5,
            );
            write!(
                svg,
                concat!(
                    "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"{}\" ",
                    "stroke-opacity=\"{:.3}\" stroke-width=\"{:.2}\" stroke-linecap=\"butt\" />"
                ),
                points[index].0,
                points[index].1,
                points[index + 1].0,
                points[index + 1].1,
                svg_rgb(curve.rgba),
                alpha,
                width
            )?;
        }
    }
    Ok(())
}

fn stroke_points_svg(
    curve: &StrokeCurve,
    doc_to_pt: f32,
    content_inset_doc: f32,
) -> Vec<(f32, f32)> {
    curve
        .points
        .iter()
        .map(|(x, y)| ((x + content_inset_doc) * doc_to_pt, y * doc_to_pt))
        .collect()
}

fn svg_path_data(curve: &StrokeCurve, points: &[(f32, f32)]) -> String {
    if !curve.path_commands.is_empty() {
        return svg_encoded_path_data(&curve.path_commands, points);
    }
    let mut data = String::new();
    let _ = write!(data, "M {:.2} {:.2}", points[0].0, points[0].1);
    if curve.preserve_vertices || points.len() == 2 {
        for point in points.iter().skip(1) {
            let _ = write!(data, " L {:.2} {:.2}", point.0, point.1);
        }
    } else if is_bezier_point_count(points.len()) {
        for segment_index in 0..((points.len() - 1) / 3) {
            let c1 = points[segment_index * 3 + 1];
            let c2 = points[segment_index * 3 + 2];
            let end = points[segment_index * 3 + 3];
            let _ = write!(
                data,
                " C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2}",
                c1.0, c1.1, c2.0, c2.1, end.0, end.1
            );
        }
    } else {
        for (c1, c2, end) in smoothed_cubic_segments(points) {
            let _ = write!(
                data,
                " C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2}",
                c1.0, c1.1, c2.0, c2.1, end.0, end.1
            );
        }
    }
    data
}

fn svg_encoded_path_data(commands: &[u8], points: &[(f32, f32)]) -> String {
    let mut data = String::new();
    let mut index = 0usize;
    for command in commands {
        match *command {
            0 => {
                let Some(point) = points.get(index) else {
                    return data;
                };
                let _ = write!(data, "M {:.2} {:.2}", point.0, point.1);
                index += 1;
            }
            1 => {
                let Some(point) = points.get(index) else {
                    return data;
                };
                let _ = write!(data, " L {:.2} {:.2}", point.0, point.1);
                index += 1;
            }
            3 => {
                let (Some(c1), Some(c2), Some(end)) = (
                    points.get(index),
                    points.get(index + 1),
                    points.get(index + 2),
                ) else {
                    return data;
                };
                let _ = write!(
                    data,
                    " C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2}",
                    c1.0, c1.1, c2.0, c2.1, end.0, end.1
                );
                index += 3;
            }
            _ => return data,
        }
    }
    data
}

fn svg_rgb(rgba: [u8; 4]) -> String {
    format!("rgb({},{},{})", rgba[0], rgba[1], rgba[2])
}

fn svg_blend_attr(multiply: bool) -> &'static str {
    if multiply {
        " style=\"mix-blend-mode:multiply\""
    } else {
        ""
    }
}

fn svg_dash_attr(curve: &StrokeCurve, doc_to_pt: f32) -> String {
    let Some(pattern) = curve.dash_pattern else {
        return String::new();
    };
    let width = (curve.width * doc_to_pt).max(0.1);
    let (on, off) = dash_lengths(pattern, curve.style, width);
    format!(" stroke-dasharray=\"{on:.2} {off:.2}\"")
}

fn svg_font_family(style: &TextStyle) -> &'static str {
    let lower = style.font_name.to_ascii_lowercase();
    if lower.contains("times") {
        "Times New Roman, Times, serif"
    } else if lower.contains("typewriter") || lower.contains("courier") || lower.contains("mono") {
        "Courier, monospace"
    } else {
        "Helvetica, Arial, sans-serif"
    }
}

fn svg_text_decoration(style: &TextStyle) -> &'static str {
    match (
        style.underline,
        style.strikethrough || style.checklist_checked,
    ) {
        (true, true) => "underline line-through",
        (true, false) => "underline",
        (false, true) => "line-through",
        (false, false) => "none",
    }
}

fn media_mime_type(path: &str) -> &'static str {
    if path.to_ascii_lowercase().ends_with(".png") {
        "image/png"
    } else if path.to_ascii_lowercase().ends_with(".gif") {
        "image/gif"
    } else {
        "image/jpeg"
    }
}

fn escape_xml_text_char(ch: char) -> String {
    match ch {
        '&' => "&amp;".to_owned(),
        '<' => "&lt;".to_owned(),
        '>' => "&gt;".to_owned(),
        '"' => "&quot;".to_owned(),
        '\'' => "&apos;".to_owned(),
        _ => ch.to_string(),
    }
}

fn escape_xml_text(text: &str) -> String {
    text.chars().map(escape_xml_text_char).collect()
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut index = 0usize;
    while index + 3 <= data.len() {
        let block =
            ((data[index] as u32) << 16) | ((data[index + 1] as u32) << 8) | data[index + 2] as u32;
        output.push(TABLE[((block >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((block >> 12) & 0x3f) as usize] as char);
        output.push(TABLE[((block >> 6) & 0x3f) as usize] as char);
        output.push(TABLE[(block & 0x3f) as usize] as char);
        index += 3;
    }
    match data.len() - index {
        1 => {
            let block = (data[index] as u32) << 16;
            output.push(TABLE[((block >> 18) & 0x3f) as usize] as char);
            output.push(TABLE[((block >> 12) & 0x3f) as usize] as char);
            output.push('=');
            output.push('=');
        }
        2 => {
            let block = ((data[index] as u32) << 16) | ((data[index + 1] as u32) << 8);
            output.push(TABLE[((block >> 18) & 0x3f) as usize] as char);
            output.push(TABLE[((block >> 12) & 0x3f) as usize] as char);
            output.push(TABLE[((block >> 6) & 0x3f) as usize] as char);
            output.push('=');
        }
        _ => {}
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{dash_lengths, image_crop_transform};
    use crate::converter::model::ImageCrop;

    #[test]
    fn dashed_lines_have_longer_marks_than_dotted_lines() {
        let dashed = dash_lengths(1, 3, 2.0);
        let dotted = dash_lengths(2, 3, 2.0);
        assert!(dashed.0 > dotted.0);
    }

    #[test]
    fn maps_a_jpeg_crop_into_a_clipped_unit_square() {
        let crop = ImageCrop {
            x: 10.0,
            y: 20.0,
            width: 50.0,
            height: 40.0,
            reference_size: None,
        };
        assert_eq!(
            image_crop_transform(Some(crop), 100, 80),
            Some([2.0, 0.0, 0.0, 2.0, -0.2, -0.5])
        );
    }

    #[test]
    fn skips_clipping_for_a_full_jpeg_crop() {
        let crop = ImageCrop {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 80.0,
            reference_size: None,
        };
        assert_eq!(image_crop_transform(Some(crop), 100, 80), None);
    }

    #[test]
    fn crop_transforms_keep_logical_scale_and_subpixel_margins() {
        let crop = ImageCrop {
            x: 0.125,
            y: 0.0,
            width: 9.875,
            height: 5.0,
            reference_size: Some((10.0, 5.0)),
        };
        let transform = image_crop_transform(Some(crop), 20, 10).unwrap();
        assert!((transform[0] - 20.0 / 19.75).abs() < 0.000001);
        assert!((transform[4] + 0.25 / 19.75).abs() < 0.000001);
        let full = ImageCrop {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 5.0,
            reference_size: Some((10.0, 5.0)),
        };
        assert_eq!(image_crop_transform(Some(full), 20, 10), None);
    }

    #[test]
    fn image_cache_distinguishes_logical_coordinate_spaces() {
        let mut media = super::MediaImage {
            page_index: 0,
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 50.0,
            rotation_degrees: 0.0,
            flipped_horizontal: false,
            flipped_vertical: false,
            relative_path: "image.png".into(),
            z_index: 0,
            crop: Some(ImageCrop {
                x: 1.0,
                y: 1.0,
                width: 4.0,
                height: 2.0,
                reference_size: Some((10.0, 5.0)),
            }),
        };
        let first = super::image_cache_key(b"same image", &media, false);
        media.crop.as_mut().unwrap().reference_size = Some((20.0, 10.0));
        assert_ne!(first, super::image_cache_key(b"same image", &media, false));
    }

    fn stroke(
        points: Vec<(f32, f32)>,
        preserve_vertices: bool,
        style: u8,
        width: f32,
    ) -> super::StrokeCurve {
        super::StrokeCurve {
            page_index: 0,
            points,
            preserve_vertices,
            path_commands: Vec::new(),
            fill_path: false,
            width,
            rgba: [0, 0, 0, 255],
            style,
            dash_pattern: None,
            pressures: vec![1.0],
            fractional_widths: vec![1.0],
            opacities: Vec::new(),
        }
    }

    #[test]
    fn pencil_polygon_keeps_straight_edges_in_pdf_and_svg() {
        let curve = stroke(
            vec![(0.0, 0.0), (100.0, 0.0), (50.0, 100.0), (0.0, 0.0)],
            true,
            super::STROKE_STYLE_PENCIL,
            2.0,
        );
        let mut svg = String::new();
        super::draw_svg_pencil_curve(&mut svg, &curve, 1.0, 0.0).unwrap();
        assert_eq!(svg.matches("<line").count(), 3);
        assert!(!svg.contains(" C "));
        let mut operations = Vec::new();
        super::draw_pencil_curve(
            &mut operations,
            &mut lopdf::Document::new(),
            &mut lopdf::Dictionary::new(),
            &mut std::collections::BTreeMap::new(),
            &curve,
            1.0,
            0.0,
            200.0,
        );
        assert_eq!(operations.iter().filter(|op| op.operator == "l").count(), 3);
        assert!(!operations.iter().any(|op| op.operator == "c"));
    }

    #[test]
    fn variable_width_polygon_keeps_straight_edges() {
        let mut curve = stroke(
            vec![(0.0, 0.0), (100.0, 0.0), (50.0, 100.0), (0.0, 0.0)],
            true,
            3,
            2.0,
        );
        curve.pressures = vec![0.5, 1.0, 2.0, 0.5];
        let mut svg = String::new();
        super::draw_svg_pressure_pen_curve(&mut svg, &curve, 1.0, 0.0).unwrap();
        assert_eq!(svg.matches("<line").count(), 3);
        let mut operations = Vec::new();
        super::draw_pressure_pen_curve(
            &mut operations,
            &mut lopdf::Document::new(),
            &mut lopdf::Dictionary::new(),
            &mut std::collections::BTreeMap::new(),
            &curve,
            1.0,
            0.0,
            200.0,
        );
        assert_eq!(operations.iter().filter(|op| op.operator == "l").count(), 3);
    }

    #[test]
    fn actual_pencil_cubics_remain_curved() {
        let curve = stroke(
            vec![(0.0, 0.0), (0.0, 100.0), (100.0, 100.0), (100.0, 0.0)],
            false,
            super::STROKE_STYLE_PENCIL,
            2.0,
        );
        let mut svg = String::new();
        super::draw_svg_pencil_curve(&mut svg, &curve, 1.0, 0.0).unwrap();
        assert_eq!(svg.matches(" C ").count(), 1);
        assert!(!svg.contains("<line"));
    }

    #[test]
    fn small_pen_marks_and_erased_outline_fragments_are_retained() {
        for mut curve in [
            stroke(vec![(10.0, 10.0)], false, 3, 0.6),
            stroke(vec![(10.0, 10.0), (10.5, 10.0)], true, 3, 0.6),
            stroke(
                vec![(10.0, 10.0), (11.0, 10.0), (11.0, 11.0), (10.0, 11.0)],
                true,
                3,
                0.6,
            ),
        ] {
            if curve.points.len() == 4 {
                curve.fill_path = true;
                curve.path_commands = vec![0, 1, 1, 1];
            }
            let mut svg = String::new();
            super::draw_svg_pen_curve(&mut svg, &curve, 1.0, 0.0, 200.0).unwrap();
            assert!(!svg.is_empty());
            let mut operations = Vec::new();
            super::draw_pen_curve(
                &mut operations,
                &mut lopdf::Document::new(),
                &mut lopdf::Dictionary::new(),
                &mut std::collections::BTreeMap::new(),
                &curve,
                1.0,
                0.0,
                200.0,
            );
            assert!(operations
                .iter()
                .any(|op| op.operator == "S" || op.operator == "f"));
        }
    }
}
