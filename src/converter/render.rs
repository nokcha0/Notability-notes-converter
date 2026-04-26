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
use image::GenericImageView;
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

    let font_id = output.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let pages_id = output.new_object_id();
    let page_count = page_count_for(note);
    let mut page_ids = Vec::with_capacity(page_count);
    for page_index in 0..page_count {
        let page_id = write_page(
            &mut output,
            &mut bundle,
            note,
            &loaded_pdfs,
            font_id,
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

fn write_page(
    output: &mut Document,
    bundle: &mut ZipArchive<File>,
    note: &NoteDocument,
    loaded_pdfs: &BTreeMap<String, Document>,
    font_id: ObjectId,
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
            font_id,
            doc_to_pt,
            content_inset_doc,
            line_spacing_doc,
        );
    }
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
        operations.extend([
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![
                    width.into(),
                    0.into(),
                    0.into(),
                    height.into(),
                    x.into(),
                    (note.export_height_pt - y - height).into(),
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

    let mut resources = dictionary! {
        "Font" => dictionary! { "F1" => font_id },
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
    _font_id: ObjectId,
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) {
    let mut cursor_x_doc = 0.0f32;
    let mut cursor_y_doc = DEFAULT_TEXT_TOP_DOC;
    let styles = build_style_map(note);
    for (char_index, ch) in note.text.chars().enumerate() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            cursor_x_doc = 0.0;
            cursor_y_doc += line_spacing_doc;
            continue;
        }
        let style = styles.get(char_index).cloned().unwrap_or(TextStyle {
            font_size: 12.0,
            color: [0, 0, 0, 255],
        });
        let font_pt = style.font_size * doc_to_pt;
        let char_width_doc = style.font_size * helvetica_width_factor(ch);
        if content_inset_doc + cursor_x_doc + char_width_doc
            > note.page_width_doc - content_inset_doc
        {
            cursor_x_doc = 0.0;
            cursor_y_doc += line_spacing_doc;
        }
        let page_index = (cursor_y_doc / note.page_height_doc).floor() as usize;
        if page_index == 0 {
            let x = (content_inset_doc + cursor_x_doc) * doc_to_pt;
            let y = note.export_height_pt - (cursor_y_doc * doc_to_pt + font_pt);
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
                Operation::new("Tf", vec!["F1".into(), font_pt.into()]),
                Operation::new("Td", vec![x.into(), y.into()]),
                Operation::new("Tj", vec![Object::string_literal(ch.to_string())]),
                Operation::new("ET", vec![]),
            ]);
        }
        cursor_x_doc += char_width_doc;
    }
}

fn build_style_map(note: &NoteDocument) -> Vec<TextStyle> {
    let mut styles = vec![
        TextStyle {
            font_size: 12.0,
            color: [0, 0, 0, 255],
        };
        note.text.chars().count()
    ];
    for span in &note.text_spans {
        for index in span.start..(span.start + span.length) {
            if let Some(slot) = styles.get_mut(index) {
                *slot = span.style.clone();
            }
        }
    }
    styles
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
    let image = image::load_from_memory(&data)?;
    let (width, height) = image.dimensions();
    let rgb = image.to_rgb8();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(rgb.as_raw())?;
    let content = encoder.finish()?;
    Ok(output.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => width as i64,
            "Height" => height as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "FlateDecode",
        },
        content,
    )))
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
        draw_path(operations, &points);
        operations.push(Operation::new("S", vec![]));
    }
    operations.push(Operation::new("Q", vec![]));
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
}

fn draw_path(operations: &mut Vec<Operation>, points: &[(f32, f32)]) {
    operations.push(Operation::new("m", vec![points[0].0.into(), points[0].1.into()]));
    if is_bezier_point_count(points.len()) {
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
    } else if points.len() == 2 {
        operations.push(Operation::new("l", vec![points[1].0.into(), points[1].1.into()]));
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
    let styles = build_style_map(note);
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
            &styles,
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
    styles: &[TextStyle],
    doc_to_pt: f32,
    content_inset_doc: f32,
    line_spacing_doc: f32,
) -> Result<()> {
    let mut cursor_x_doc = 0.0f32;
    let mut cursor_y_doc = DEFAULT_TEXT_TOP_DOC;
    for (char_index, ch) in note.text.chars().enumerate() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            cursor_x_doc = 0.0;
            cursor_y_doc += line_spacing_doc;
            continue;
        }
        let style = styles.get(char_index).cloned().unwrap_or(TextStyle {
            font_size: 12.0,
            color: [0, 0, 0, 255],
        });
        let font_pt = style.font_size * doc_to_pt;
        let char_width_doc = style.font_size * helvetica_width_factor(ch);
        if content_inset_doc + cursor_x_doc + char_width_doc
            > note.page_width_doc - content_inset_doc
        {
            cursor_x_doc = 0.0;
            cursor_y_doc += line_spacing_doc;
        }
        let char_page_index = (cursor_y_doc / note.page_height_doc).floor() as usize;
        if char_page_index == page_index {
            let x = (content_inset_doc + cursor_x_doc) * doc_to_pt;
            let y = cursor_y_doc * doc_to_pt + font_pt;
            write!(
                svg,
                concat!(
                    "<text x=\"{:.2}\" y=\"{:.2}\" font-family=\"Helvetica\" font-size=\"{:.2}\" ",
                    "fill=\"{}\" fill-opacity=\"{:.3}\" xml:space=\"preserve\">{}</text>"
                ),
                x,
                y,
                font_pt,
                svg_rgb(style.color),
                style.color[3] as f32 / 255.0,
                escape_xml_text_char(ch)
            )?;
        }
        cursor_x_doc += char_width_doc;
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
        write!(
            svg,
            concat!(
                "<image x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" ",
                "href=\"data:{};base64,{}\" preserveAspectRatio=\"none\"/>"
            ),
            (content_inset_doc + media.x) * doc_to_pt,
            media.y * doc_to_pt,
            media.width * doc_to_pt,
            media.height * doc_to_pt,
            media_mime_type(&media.relative_path),
            base64_encode(&data)
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
                "stroke-width=\"{:.2}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"{} />"
            ),
            svg_path_data(&points),
            svg_rgb(curve.rgba),
            alpha,
            (curve.width * doc_to_pt).max(0.1),
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

fn svg_path_data(points: &[(f32, f32)]) -> String {
    let mut data = String::new();
    let _ = write!(data, "M {:.2} {:.2}", points[0].0, points[0].1);
    if is_bezier_point_count(points.len()) {
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
    } else if points.len() == 2 {
        let _ = write!(data, " L {:.2} {:.2}", points[1].0, points[1].1);
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
