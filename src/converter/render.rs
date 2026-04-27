use super::model::{
    NoteDocument, OutputFormat, StrokeCurve, TextStyle, DEFAULT_CONTENT_INSET_RATIO,
    DEFAULT_TEXT_TOP_DOC, STROKE_STYLE_HIGHLIGHTER, STROKE_STYLE_NOT_EXPORTED,
    STROKE_STYLE_PENCIL, CURVE_SMOOTHING_TENSION,
};
use super::note::read_zip_entry;
use super::util::{page_count_for, parse_line_spacing_doc};
use crate::pdf::{add_form_xobject, page_box, page_id_by_index};
use crate::Result;
use flate2::{write::ZlibEncoder, Compression};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ExtendedColorType, GenericImageView, ImageFormat};
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use zip::ZipArchive;

pub(crate) fn write_note_output(
    note: &NoteDocument,
    note_path: &Path,
    output_path: &Path,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Pdf => write_lopdf_pdf(note, note_path, output_path),
        OutputFormat::Svg => write_svg(note, note_path, output_path),
    }
}

fn write_lopdf_pdf(note: &NoteDocument, note_path: &Path, output_path: &Path) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let source_file = File::open(note_path)?;
    let mut bundle = ZipArchive::new(source_file)?;
    let mut output = Document::with_version("1.5");
    let mut max_id = 1u32;
    let mut loaded_pdfs: BTreeMap<String, Document> = BTreeMap::new();
    for relative_path in note
        .pdf_pages
        .iter()
        .map(|page| page.relative_path.clone())
        .collect::<BTreeSet<_>>()
    {
        let data = read_zip_entry(&mut bundle, &(note.bundle_root.clone() + &relative_path))?;
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
            &mut bundle,
            note,
            &loaded_pdfs,
            &fonts,
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
    output.prune_objects();
    output.renumber_objects();
    output.save(output_path)?;
    Ok(())
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

fn write_page(
    output: &mut Document,
    bundle: &mut ZipArchive<File>,
    note: &NoteDocument,
    loaded_pdfs: &BTreeMap<String, Document>,
    fonts: &PdfFonts,
    pages_id: ObjectId,
    page_index: usize,
) -> Result<ObjectId> {
    let mut operations = Vec::new();
    let mut xobjects = Dictionary::new();
    let mut ext_gstates = Dictionary::new();
    let mut ext_cache: BTreeMap<(u16, bool), String> = BTreeMap::new();
    let doc_to_pt = note.export_width_pt / note.page_width_doc;
    let content_inset_doc = note.page_width_doc * DEFAULT_CONTENT_INSET_RATIO;
    let line_spacing_doc =
        parse_line_spacing_doc(note.line_style.as_deref(), note.page_width_doc, note.export_width_pt);
    let pdf_page = note.pdf_pages.iter().find(|page| page.page_index == page_index);
    if let Some(pdf_page) = pdf_page {
        if let Some(base_doc) = loaded_pdfs.get(&pdf_page.relative_path) {
            let page_id = page_id_by_index(base_doc, pdf_page.source_page_index)?;
            let bbox = page_box(base_doc, page_id)?;
            let form_id = add_form_xobject(output, base_doc, page_id, bbox)?;
            xobjects.set("Base", form_id);
            let width = bbox[2] - bbox[0];
            let height = bbox[3] - bbox[1];
            let sx = note.export_width_pt / width;
            let sy = note.export_height_pt / height;
            operations.extend([
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        sx.into(),
                        0.into(),
                        0.into(),
                        sy.into(),
                        (-bbox[0] * sx).into(),
                        (-bbox[1] * sy).into(),
                    ],
                ),
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

    if page_index == 0 {
        draw_text(
            &mut operations,
            note,
            page_index,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        );
    }
    draw_text_blocks(
        &mut operations,
        note,
        page_index,
        doc_to_pt,
        content_inset_doc,
        line_spacing_doc,
    );
    for (image_index, media) in note
        .media_images
        .iter()
        .filter(|media| media.page_index == page_index)
        .enumerate()
    {
        let name = format!("Im{image_index}");
        let image_id = add_image_xobject(output, bundle, note, media)?;
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
                    width.into(),
                    0.into(),
                    0.into(),
                    height.into(),
                    (-width * 0.5).into(),
                    (-height * 0.5).into(),
                ],
            ),
            Operation::new("Do", vec![name.into()]),
            Operation::new("Q", vec![]),
        ]);
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
            vec![0.into(), 0.into(), note.export_width_pt.into(), note.export_height_pt.into()],
        ),
        Operation::new("f", vec![]),
        Operation::new("Q", vec![]),
    ]);
    if !note.line_style.as_deref().unwrap_or("").starts_with("Lines") {
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

fn draw_text_blocks(
    operations: &mut Vec<Operation>,
    note: &NoteDocument,
    page_index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) {
    for block in note.text_blocks.iter().filter(|block| block.page_index == page_index) {
        let _block_height_doc = block.height;
        draw_text_content(
            operations,
            &block.text,
            &block.text_spans,
            page_index,
            note.page_height_doc,
            note.export_height_pt,
            block.x,
            block.y,
            block.width,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        );
    }
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
        let style = styles.get(char_index).cloned().unwrap_or_else(default_text_style);
        line_height_doc = line_height_doc.max(style.font_size * style.line_spacing_multiplier.max(1.0));
        if at_line_start {
            draw_pdf_list_marker(
                operations,
                &style,
                origin_x_doc,
                origin_y_doc + cursor_y_doc,
                doc_to_pt,
                content_inset_doc,
                export_height_pt,
            );
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
    let bold = style.bold || lower.contains("impact") || lower.contains("chalkduster");
    match (family, bold, style.italic) {
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
            Operation::new("Tf", vec![pdf_font_resource(style).into(), (style.font_size * doc_to_pt).into()]),
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

fn add_image_xobject(
    output: &mut Document,
    bundle: &mut ZipArchive<File>,
    note: &NoteDocument,
    media: &super::model::MediaImage,
) -> Result<ObjectId> {
    let path = note.bundle_root.clone() + &media.relative_path;
    let data = read_zip_entry(bundle, &path)?;
    let image = load_media_image(&data, media)?;
    let (width, height) = image.dimensions();
    let rgb = image.to_rgb8();
    let mut content = Vec::new();
    let is_jpeg = media.relative_path.to_ascii_lowercase().ends_with(".jpg")
        || media.relative_path.to_ascii_lowercase().ends_with(".jpeg");
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
    Ok(output.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => width as i64,
            "Height" => height as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => filter,
        },
        content,
    )))
}

fn load_media_image(data: &[u8], media: &super::model::MediaImage) -> Result<DynamicImage> {
    let image = image::load_from_memory(data)?;
    let Some(crop) = media.crop else {
        return Ok(image);
    };
    let (image_width, image_height) = image.dimensions();
    let x = crop.x.max(0.0).floor() as u32;
    let y = crop.y.max(0.0).floor() as u32;
    if x >= image_width || y >= image_height {
        return Ok(image);
    }
    let width = crop.width.max(1.0).ceil() as u32;
    let height = crop.height.max(1.0).ceil() as u32;
    let width = width.min(image_width - x);
    let height = height.min(image_height - y);
    Ok(image.crop_imm(x, y, width, height))
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
            if curve.style == STROKE_STYLE_PENCIL {
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

fn curve_has_variable_width(curve: &StrokeCurve) -> bool {
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
    if is_bezier_point_count(points.len()) {
        let sample_count = ((points.len() - 1) / 3) + 1;
        let pressures = resample_values(&curve.pressures, sample_count);
        let fracs = resample_values(&curve.fractional_widths, sample_count);
        for segment_index in 0..((points.len() - 1) / 3) {
            let width = pen_width(
                curve.width * doc_to_pt,
                (pressures[segment_index] + pressures[segment_index + 1]) * 0.5,
                (fracs[segment_index] + fracs[segment_index + 1]) * 0.5,
            );
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
            let width = pen_width(
                curve.width * doc_to_pt,
                (pressures[index] + pressures[index + 1]) * 0.5,
                (fracs[index] + fracs[index + 1]) * 0.5,
            );
            push_stroke_state(operations, curve, width, &gs);
            operations.extend([
                Operation::new("m", vec![points[index].0.into(), points[index].1.into()]),
                Operation::new("l", vec![points[index + 1].0.into(), points[index + 1].1.into()]),
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
    if curve.points.len() < 2 {
        return;
    }
    let points = stroke_points_pt(curve, doc_to_pt, content_inset_doc, page_height);
    if is_bezier_point_count(points.len()) {
        let sample_count = ((points.len() - 1) / 3) + 1;
        let pressures = resample_values(&curve.pressures, sample_count);
        let fracs = resample_values(&curve.fractional_widths, sample_count);
        for segment_index in 0..((points.len() - 1) / 3) {
            let pressure = (pressures[segment_index] + pressures[segment_index + 1]) / 2.0;
            let frac = (fracs[segment_index] + fracs[segment_index + 1]) / 2.0;
            let width = pencil_width(curve.width * doc_to_pt, pressure, frac);
            let alpha =
                (curve.rgba[3] as f32 / 255.0) * (0.18 + 0.34 * pressure).clamp(0.14, 0.96);
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
            let alpha =
                (curve.rgba[3] as f32 / 255.0) * (0.18 + 0.34 * pressure).clamp(0.14, 0.96);
            let gs = ext_gstate_name(output, ext_gstates, ext_cache, alpha, false);
            push_stroke_state(operations, curve, width, &gs);
            operations.extend([
                Operation::new("m", vec![points[index].0.into(), points[index].1.into()]),
                Operation::new("l", vec![points[index + 1].0.into(), points[index + 1].1.into()]),
                Operation::new("S", vec![]),
                Operation::new("Q", vec![]),
            ]);
        }
    }
}

fn push_stroke_state(
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
        Operation::new("J", vec![1.into()]),
        Operation::new("j", vec![1.into()]),
    ]);
    if let Some(pattern) = curve.dash_pattern {
        let (on, off) = dash_lengths(pattern, curve.style, width);
        let dash = vec![on.into(), off.into()];
        operations.push(Operation::new("d", vec![Object::Array(dash), 0.into()]));
    }
}

fn dash_lengths(pattern: u8, style: u8, width: f32) -> (f32, f32) {
    match (pattern, style == STROKE_STYLE_HIGHLIGHTER) {
        (1, true) => ((width * 2.0).max(2.0), (width * 1.4).max(1.4)),
        (2, true) => ((width * 0.12).max(0.12), (width * 1.8).max(1.8)),
        (1, false) => ((width * 0.12).max(0.12), (width * 2.1).max(1.2)),
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
    operations.push(Operation::new("m", vec![points[0].0.into(), points[0].1.into()]));
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

fn smoothed_cubic_segments(points: &[(f32, f32)]) -> Vec<((f32, f32), (f32, f32), (f32, f32))> {
    if points.len() < 3 {
        return Vec::new();
    }
    let scale = CURVE_SMOOTHING_TENSION / 6.0;
    let mut segments = Vec::with_capacity(points.len() - 1);
    for index in 0..points.len() - 1 {
        let p0 = if index > 0 { points[index - 1] } else { points[index] };
        let p1 = points[index];
        let p2 = points[index + 1];
        let p3 = if index + 2 < points.len() { points[index + 2] } else { p2 };
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
        .map(|(x, y)| ((x + content_inset_doc) * doc_to_pt, page_height - y * doc_to_pt))
        .collect()
}

fn pencil_width(base_width: f32, pressure: f32, frac: f32) -> f32 {
    let pressure = pressure.clamp(0.05, 2.5);
    let frac = frac.clamp(0.15, 3.0);
    (base_width * frac * (0.45 + 0.38 * pressure.sqrt())).max(0.1)
}

fn pen_width(base_width: f32, pressure: f32, frac: f32) -> f32 {
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

fn write_svg(note: &NoteDocument, note_path: &Path, output_path: &Path) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let source_file = File::open(note_path)?;
    let mut bundle = ZipArchive::new(source_file)?;
    let page_count = page_count_for(note);
    let total_height = note.export_height_pt * page_count as f32;
    let doc_to_pt = note.export_width_pt / note.page_width_doc;
    let content_inset_doc = note.page_width_doc * DEFAULT_CONTENT_INSET_RATIO;
    let line_spacing_doc =
        parse_line_spacing_doc(note.line_style.as_deref(), note.page_width_doc, note.export_width_pt);
    let mut svg = String::new();
    write!(
        svg,
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{:.2}\" height=\"{:.2}\" ",
            "viewBox=\"0 0 {:.2} {:.2}\">"
        ),
        note.export_width_pt,
        total_height,
        note.export_width_pt,
        total_height
    )?;
    if !note.pdf_pages.is_empty() {
        svg.push_str("<!-- Imported PDF base pages are not rendered in SVG output. -->");
    }
    for page_index in 0..page_count {
        let page_origin_y = page_index as f32 * note.export_height_pt;
        write!(
            svg,
            "<g id=\"page-{}\" transform=\"translate(0,{:.2})\">",
            page_index + 1,
            page_origin_y
        )?;
        draw_svg_background(
            &mut svg,
            note,
            page_index,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        )?;
        draw_svg_text(
            &mut svg,
            note,
            page_index,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        )?;
        draw_svg_text_blocks(
            &mut svg,
            note,
            page_index,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        )?;
        draw_svg_images(&mut svg, &mut bundle, note, page_index, doc_to_pt, content_inset_doc)?;
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
    fs::write(output_path, svg)?;
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
        note.export_width_pt,
        note.export_height_pt
    )?;
    if note.pdf_pages.iter().any(|page| page.page_index == page_index) {
        return Ok(());
    }
    if !note.line_style.as_deref().unwrap_or("").starts_with("Lines") {
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

fn draw_svg_text_blocks(
    svg: &mut String,
    note: &NoteDocument,
    page_index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) -> Result<()> {
    for block in note.text_blocks.iter().filter(|block| block.page_index == page_index) {
        let _block_height_doc = block.height;
        draw_svg_text_content(
            svg,
            note,
            page_index,
            &block.text,
            &block.text_spans,
            block.x,
            block.y,
            block.width,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        )?;
    }
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
        let style = styles.get(char_index).cloned().unwrap_or_else(default_text_style);
        line_height_doc = line_height_doc.max(style.font_size * style.line_spacing_multiplier.max(1.0));
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
    let baseline_y =
        (cursor_y_doc - marker_page_index as f32 * note.page_height_doc) * doc_to_pt
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

fn draw_svg_images(
    svg: &mut String,
    bundle: &mut ZipArchive<File>,
    note: &NoteDocument,
    page_index: usize,
    doc_to_pt: f32,
    content_inset_doc: f32,
) -> Result<()> {
    for media in note
        .media_images
        .iter()
        .filter(|media| media.page_index == page_index)
    {
        let path = note.bundle_root.clone() + &media.relative_path;
        let data = read_zip_entry(bundle, &path)?;
        let (mime_type, encoded_data) = if media.crop.is_some() {
            let image = load_media_image(&data, media)?;
            let mut cursor = std::io::Cursor::new(Vec::new());
            image.write_to(&mut cursor, ImageFormat::Png)?;
            ("image/png", cursor.into_inner())
        } else {
            (media_mime_type(&media.relative_path), data)
        };
        let x = (content_inset_doc + media.x) * doc_to_pt;
        let y = media.y * doc_to_pt;
        let width = media.width * doc_to_pt;
        let height = media.height * doc_to_pt;
        let transform = if media.rotation_degrees.abs() > f32::EPSILON {
            format!(
                " transform=\"rotate({:.4} {:.2} {:.2})\"",
                media.rotation_degrees,
                x + width * 0.5,
                y + height * 0.5
            )
        } else {
            String::new()
        };
        write!(
            svg,
            concat!(
                "<image x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" ",
                "href=\"data:{};base64,{}\" preserveAspectRatio=\"none\"{}/>"
            ),
            x,
            y,
            width,
            height,
            mime_type,
            base64_encode(&encoded_data),
            transform
        )?;
    }
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
            if curve.style == STROKE_STYLE_PENCIL {
                draw_svg_pencil_curve(svg, curve, doc_to_pt, content_inset_doc)?;
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
    page_height: f32,
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
    if points.len() == 1 {
        let radius = (curve.width * doc_to_pt / 2.0).max(0.5);
        write!(
            svg,
            concat!(
                "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"{}\" fill-opacity=\"{:.3}\"{} />"
            ),
            points[0].0,
            points[0].1.clamp(0.0, page_height),
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
    if curve.points.len() < 2 {
        return Ok(());
    }
    let points = stroke_points_svg(curve, doc_to_pt, content_inset_doc);
    if is_bezier_point_count(points.len()) {
        let sample_count = ((points.len() - 1) / 3) + 1;
        let pressures = resample_values(&curve.pressures, sample_count);
        let fracs = resample_values(&curve.fractional_widths, sample_count);
        for segment_index in 0..((points.len() - 1) / 3) {
            let pressure = (pressures[segment_index] + pressures[segment_index + 1]) / 2.0;
            let frac = (fracs[segment_index] + fracs[segment_index + 1]) / 2.0;
            let width = pencil_width(curve.width * doc_to_pt, pressure, frac);
            let alpha =
                (curve.rgba[3] as f32 / 255.0) * (0.18 + 0.34 * pressure).clamp(0.14, 0.96);
            let start = points[segment_index * 3];
            let c1 = points[segment_index * 3 + 1];
            let c2 = points[segment_index * 3 + 2];
            let end = points[segment_index * 3 + 3];
            write!(
                svg,
                concat!(
                    "<path d=\"M {:.2} {:.2} C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2}\" ",
                    "fill=\"none\" stroke=\"{}\" stroke-opacity=\"{:.3}\" stroke-width=\"{:.2}\" ",
                    "stroke-linecap=\"round\" stroke-linejoin=\"round\" />"
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
            let alpha =
                (curve.rgba[3] as f32 / 255.0) * (0.18 + 0.34 * pressure).clamp(0.14, 0.96);
            write!(
                svg,
                concat!(
                    "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"{}\" ",
                    "stroke-opacity=\"{:.3}\" stroke-width=\"{:.2}\" stroke-linecap=\"round\" />"
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
    match (style.underline, style.strikethrough || style.checklist_checked) {
        (true, true) => "underline line-through",
        (true, false) => "underline",
        (false, true) => "line-through",
        (false, false) => "none",
    }
}

fn media_mime_type(path: &str) -> &'static str {
    if path.to_ascii_lowercase().ends_with(".png") {
        "image/png"
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
