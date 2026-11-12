use super::model::{
    ImageCrop, MediaImage, NoteDocument, StickyNote, StrokeCurve, TextSpan, TextStyle,
    DEFAULT_CONTENT_INSET_RATIO, STROKE_STYLE_HIGHLIGHTER, STROKE_STYLE_PENCIL,
};
use super::note::read_zip_entry;
use crate::Result;
#[cfg(not(target_arch = "wasm32"))]
use std::fs::File;
use std::io::{Read, Seek};
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use zip::ZipArchive;

mod documents;
#[cfg(test)]
mod fixtures;

// Native NTB bundles store the note as a FlatBuffers event collection. Decode only
// the stable fields needed by the renderer, while keeping every read bounds-checked.
const EVENT_DOCUMENT: u8 = 1;
const EVENT_TEXT: u8 = 8;
const EVENT_TEXT_STYLE: u8 = 12;
const EVENT_PARAGRAPH_STYLE: u8 = 13;
const EVENT_INK: u8 = 15;
const EVENT_SHAPE: u8 = 18;
const EVENT_MEDIA: u8 = 22;
const NTB_PENCIL_WIDTH_SCALE: f32 = 0.45 / 0.68;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn load_ntb_document(path: &Path) -> Result<NoteDocument> {
    read_ntb_document(ZipArchive::new(File::open(path)?)?)
}

pub(crate) fn read_ntb_document<R: Read + Seek>(mut bundle: ZipArchive<R>) -> Result<NoteDocument> {
    let bytes = read_zip_entry(&mut bundle, "noteBundle")?;
    let flat = FlatBuffer::new(&bytes);
    let root = flat.root_table().ok_or("Invalid .ntb noteBundle root")?;
    let events = flat
        .table_vector(root, 6)
        .ok_or("Invalid .ntb event collection")?;

    let mut page_width = 679.0f32;
    let mut page_height = 891.187_56f32;
    let mut line_spacing = None;
    let mut default_style = default_text_style();
    let mut media_images = Vec::new();
    let mut sticky_notes = Vec::new();
    let mut curves = Vec::new();

    // Paper settings may occur after positioned objects in an event collection.
    let ordered_events = events
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, event)| {
            flat.field(*event, 4).and_then(|field| flat.u8(field)) == Some(EVENT_DOCUMENT)
        })
        .chain(events.iter().copied().enumerate().filter(|(_, event)| {
            flat.field(*event, 4).and_then(|field| flat.u8(field)) != Some(EVENT_DOCUMENT)
        }));
    for (event_index, event) in ordered_events {
        let Some(event_type) = flat.field(event, 4).and_then(|field| flat.u8(field)) else {
            continue;
        };
        let Some(payload) = flat.offset_field(event, 5) else {
            continue;
        };
        match event_type {
            EVENT_DOCUMENT => {
                if let Some(font_name) = flat.string_field(payload, 4) {
                    default_style.font_name = font_name.to_owned();
                }
                if let Some(font_size) = flat.field(payload, 5).and_then(|field| flat.f32(field)) {
                    default_style.font_size = font_size;
                }
                if let Some(paper) = flat
                    .offset_field(payload, 1)
                    .and_then(|value| flat.offset_field(value, 0))
                {
                    if let Some((width, height)) = flat.pair_field(paper, 3) {
                        if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 {
                            page_width = width;
                            page_height = height;
                        }
                    }
                    line_spacing = flat
                        .offset_field(paper, 0)
                        .and_then(|style| flat.field(style, 1))
                        .and_then(|field| flat.f32(field))
                        .filter(|spacing| spacing.is_finite() && *spacing > 0.0);
                }
            }
            EVENT_MEDIA => {
                let inset = page_width * DEFAULT_CONTENT_INSET_RATIO;
                if let Some(image) = parse_media_image(&flat, payload, event_index, inset) {
                    media_images.push(image);
                } else if let Some(sticky) = parse_sticky_note(&flat, payload, event_index, inset) {
                    sticky_notes.push(sticky);
                }
            }
            EVENT_INK => {
                let inset = page_width * DEFAULT_CONTENT_INSET_RATIO;
                curves.extend(parse_ink(&flat, payload, inset));
            }
            EVENT_SHAPE => {
                let inset = page_width * DEFAULT_CONTENT_INSET_RATIO;
                if let Some(curve) = parse_shape(&flat, payload, inset) {
                    curves.push(curve);
                }
            }
            _ => {}
        }
    }

    let (text, text_spans, text_blocks) = documents::reconstruct_text(
        &flat,
        &events,
        default_style,
        page_width * DEFAULT_CONTENT_INSET_RATIO,
    )?;
    let pdf_pages = documents::pdf_backgrounds(&mut bundle)?;
    let line_style = line_spacing.map(|spacing| format!("Lines:false:false:{}", spacing / 72.0));
    Ok(NoteDocument {
        bundle_root: String::new(),
        page_width_doc: page_width,
        page_height_doc: page_height,
        export_width_pt: page_width,
        export_height_pt: page_height,
        layout_width_pt: page_width,
        line_style,
        text,
        text_spans,
        text_blocks,
        media_images,
        sticky_notes,
        pdf_pages,
        curves,
    })
}

#[derive(Clone, Debug, Default)]
struct TextStylePatch {
    font_size: Option<f32>,
    font_name: Option<String>,
    color: Option<[u8; 4]>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strikethrough: Option<bool>,
}

#[derive(Clone, Debug)]
struct TextStyleEvent {
    start: usize,
    end: usize,
    patch: TextStylePatch,
}

#[derive(Clone, Debug, Default)]
struct ParagraphStylePatch {
    indent_level: Option<usize>,
    line_spacing_multiplier: Option<f32>,
    decoration_style: Option<i64>,
}

#[derive(Clone, Debug)]
struct ParagraphStyleEvent {
    start: usize,
    end: usize,
    patch: ParagraphStylePatch,
}

fn parse_text_style_event(flat: &FlatBuffer<'_>, payload: usize) -> TextStyleEvent {
    let patch = TextStylePatch {
        font_size: nested_f32(flat, payload, 8, 0),
        font_name: nested_string(flat, payload, 7, 0).map(str::to_owned),
        color: flat
            .offset_field(payload, 9)
            .and_then(|value| flat.field(value, 0))
            .and_then(|field| flat.bytes::<4>(field)),
        bold: nested_bool(flat, payload, 3, 0),
        italic: nested_bool(flat, payload, 4, 0),
        underline: nested_bool(flat, payload, 5, 0),
        strikethrough: nested_bool(flat, payload, 11, 0),
    };
    TextStyleEvent {
        start: position_index(flat, payload, 0),
        end: position_index(flat, payload, 1),
        patch,
    }
}

fn parse_paragraph_style_event(flat: &FlatBuffer<'_>, payload: usize) -> ParagraphStyleEvent {
    ParagraphStyleEvent {
        start: position_index(flat, payload, 0),
        end: position_index(flat, payload, 1),
        patch: ParagraphStylePatch {
            indent_level: nested_u8(flat, payload, 2, 0).map(usize::from),
            line_spacing_multiplier: nested_f32(flat, payload, 4, 0),
            decoration_style: nested_u8(flat, payload, 5, 0).map(i64::from),
        },
    }
}

fn build_text_spans(
    text: &str,
    default_style: TextStyle,
    style_events: &[TextStyleEvent],
    paragraph_events: &[ParagraphStyleEvent],
) -> Vec<TextSpan> {
    let count = text.chars().count();
    let mut styles = vec![default_style; count];
    for event in style_events {
        let end = normalize_range_end(event.end, count);
        for style in styles.iter_mut().take(end).skip(event.start.min(count)) {
            if let Some(value) = event.patch.font_size {
                style.font_size = value;
            }
            if let Some(value) = &event.patch.font_name {
                style.font_name = value.clone();
            }
            if let Some(value) = event.patch.color {
                style.color = value;
            }
            if let Some(value) = event.patch.bold {
                style.bold = value;
            }
            if let Some(value) = event.patch.italic {
                style.italic = value;
            }
            if let Some(value) = event.patch.underline {
                style.underline = value;
            }
            if let Some(value) = event.patch.strikethrough {
                style.strikethrough = value;
            }
        }
    }
    for event in paragraph_events {
        let end = normalize_range_end(event.end, count);
        for style in styles.iter_mut().take(end).skip(event.start.min(count)) {
            if let Some(value) = event.patch.indent_level {
                style.indent_level = Some(value);
            }
            if let Some(value) = event.patch.line_spacing_multiplier {
                style.line_spacing_multiplier = value;
            }
            if let Some(value) = event.patch.decoration_style {
                style.indent_decoration_style = Some(value);
            }
        }
    }
    assign_list_numbers(text, &mut styles);

    let mut spans = Vec::new();
    let mut start = 0usize;
    while start < styles.len() {
        let mut end = start + 1;
        while end < styles.len() && styles[end] == styles[start] {
            end += 1;
        }
        spans.push(TextSpan {
            start,
            length: end - start,
            style: styles[start].clone(),
        });
        start = end;
    }
    if spans.is_empty() {
        spans.push(TextSpan {
            start: 0,
            length: 0,
            style: default_text_style(),
        });
    }
    spans
}

fn assign_list_numbers(text: &str, styles: &mut [TextStyle]) {
    let mut counters = [0i64; 16];
    let mut line_start = true;
    for (index, ch) in text.chars().enumerate() {
        if line_start {
            if let Some(style) = styles.get_mut(index) {
                if style.indent_decoration_style.unwrap_or(0) == 2 {
                    let level = style.indent_level.unwrap_or(0).min(counters.len() - 1);
                    counters[level] += 1;
                    counters
                        .iter_mut()
                        .skip(level + 1)
                        .for_each(|value| *value = 0);
                    style.indent_decoration_number = Some(counters[level]);
                }
            }
            line_start = false;
        }
        if ch == '\n' {
            line_start = true;
        }
    }
}

fn normalize_range_end(end: usize, text_len: usize) -> usize {
    if end == usize::MAX {
        text_len
    } else {
        end.min(text_len)
    }
}

fn parse_media_image(
    flat: &FlatBuffer<'_>,
    payload: usize,
    event_index: usize,
    inset: f32,
) -> Option<MediaImage> {
    let media = flat.offset_field(payload, 10)?;
    let asset = flat.offset_field(media, 0)?;
    let logical_asset_size = flat.pair_field(media, 1);
    let hash_field = flat.field(asset, 0)?;
    let hash = flat.slice(hash_field, 64)?;
    let mime = flat.string_field(asset, 2).unwrap_or("image/jpeg");
    if !mime.starts_with("image/") {
        return None;
    }
    let extension = match mime {
        "image/png" => "png",
        "image/gif" => "gif",
        _ => "jpeg",
    };
    let (x, y) = flat.pair_field(payload, 3)?;
    let (width, height) = flat.pair_field(payload, 6)?;
    let rotation_radians = flat
        .field(payload, 4)
        .and_then(|field| flat.f32(field))
        .unwrap_or(0.0);
    let (x, y) = unrotate_top_left((x - inset, y), (width, height), rotation_radians);
    let crop = flat.field(payload, 11).and_then(|field| {
        let values = flat.float_array::<4>(field)?;
        if values[2] <= 0.0 || values[3] <= 0.0 {
            return None;
        }
        if logical_asset_size.is_some_and(|(asset_width, asset_height)| {
            values[0] == 0.0
                && values[1] == 0.0
                && values[2] == asset_width
                && values[3] == asset_height
        }) {
            return None;
        }
        Some(ImageCrop {
            x: values[0],
            y: values[1],
            width: values[2],
            height: values[3],
            reference_size: logical_asset_size,
        })
    });
    Some(MediaImage {
        page_index: page_index(flat, payload, 2),
        x,
        y,
        width,
        height,
        rotation_degrees: rotation_radians.to_degrees(),
        // NTB's optional fields 19 and 20 are not legacy image-flip flags. The
        // bundled pixels are already stored in their intended orientation.
        flipped_horizontal: false,
        flipped_vertical: false,
        relative_path: format!("assets/{}.{}", hex(hash), extension),
        z_index: flat
            .field(payload, 9)
            .and_then(|field| flat.u64(field))
            .and_then(|value| i64::try_from(value).ok())
            .unwrap_or(event_index as i64),
        crop,
    })
}

/// NTB stores the visible top-left corner after rotation. The shared render
/// model stores the unrotated rectangle and rotates it around its center, so
/// recover that rectangle's top-left corner before rendering.
fn unrotate_top_left(
    rotated_top_left: (f32, f32),
    size: (f32, f32),
    rotation_radians: f32,
) -> (f32, f32) {
    let half_width = size.0 * 0.5;
    let half_height = size.1 * 0.5;
    let (sin, cos) = rotation_radians.sin_cos();
    (
        rotated_top_left.0 - half_width + half_width * cos - half_height * sin,
        rotated_top_left.1 - half_height + half_width * sin + half_height * cos,
    )
}

fn parse_sticky_note(
    flat: &FlatBuffer<'_>,
    payload: usize,
    event_index: usize,
    inset: f32,
) -> Option<StickyNote> {
    let paper = flat.offset_field(payload, 15)?;
    let (x, y) = flat.pair_field(payload, 3)?;
    let (width, height) = flat.pair_field(payload, 6)?;
    let color = flat
        .field(paper, 4)
        .and_then(|field| flat.bytes::<4>(field))
        .unwrap_or([255, 244, 142, 255]);
    let line_style = flat
        .field(paper, 1)
        .and_then(|field| flat.f32(field))
        .filter(|spacing| *spacing > 0.0)
        .map(|spacing| format!("Lines:true:false:{}", spacing / 72.0));
    Some(StickyNote {
        page_index: page_index(flat, payload, 2),
        x: x - inset,
        y,
        width,
        height,
        rotation_degrees: flat
            .field(payload, 4)
            .and_then(|field| flat.f32(field))
            .unwrap_or(0.0)
            .to_degrees(),
        color,
        line_style,
        line_start_multiple: 2.0,
        z_index: flat
            .field(payload, 9)
            .and_then(|field| flat.u64(field))
            .and_then(|value| i64::try_from(value).ok())
            .unwrap_or(event_index as i64),
    })
}

fn parse_ink(flat: &FlatBuffer<'_>, payload: usize, inset: f32) -> Vec<StrokeCurve> {
    let Some((x, y)) = flat.pair_field(payload, 1) else {
        return Vec::new();
    };
    let scale = flat
        .pair_field(payload, 3)
        .filter(|(x_scale, y_scale)| {
            x_scale.is_finite()
                && y_scale.is_finite()
                && x_scale.abs() > f32::EPSILON
                && y_scale.abs() > f32::EPSILON
        })
        .unwrap_or((1.0, 1.0));
    let Some(encoded) = flat.byte_vector(payload, 9) else {
        return Vec::new();
    };
    let origin = (x - inset, y);
    let Some(decoded) = decode_ink_path(encoded, origin, scale) else {
        return Vec::new();
    };
    let rotation = flat
        .field(payload, 2)
        .and_then(|field| flat.f32(field))
        .unwrap_or(0.0);
    let stroke_scale = (scale.0.abs() + scale.1.abs()) * 0.5;
    let tool = flat
        .field(payload, 4)
        .and_then(|field| flat.u8(field))
        .unwrap_or(0);
    let line_style = flat
        .field(payload, 5)
        .and_then(|field| flat.u8(field))
        .unwrap_or(1);
    let tool_width_scale = if tool == 1 {
        NTB_PENCIL_WIDTH_SCALE
    } else {
        1.0
    };
    let Some(width) = flat.field(payload, 8).and_then(|field| flat.f32(field)) else {
        return Vec::new();
    };
    let page_index = page_index(flat, payload, 0);
    let rgba = flat
        .field(payload, 7)
        .and_then(|field| flat.bytes::<4>(field))
        .unwrap_or([0, 0, 0, 255]);
    decoded
        .into_iter()
        .map(|mut path| {
            if rotation.abs() > f32::EPSILON {
                rotate_points(&mut path.points, origin, rotation);
            }
            StrokeCurve {
                page_index,
                points: path.points,
                preserve_vertices: path.preserve_vertices,
                path_commands: Vec::new(),
                fill_path: false,
                width: width * stroke_scale * tool_width_scale,
                rgba,
                style: tool_style(tool),
                dash_pattern: (line_style > 1).then_some(line_style - 1),
                pressures: path.pressures,
                fractional_widths: path.fractional_widths,
                opacities: path.opacities,
            }
        })
        .collect()
}

fn parse_shape(flat: &FlatBuffer<'_>, payload: usize, inset: f32) -> Option<StrokeCurve> {
    let (origin_x, origin_y) = flat.pair_field(payload, 1)?;
    let origin = (origin_x - inset, origin_y);
    let shape = flat.offset_field(payload, 5)?;
    let shape_kind = flat.field(payload, 4).and_then(|field| flat.u8(field))?;
    let rotation = flat
        .field(payload, 2)
        .and_then(|field| flat.f32(field))
        .unwrap_or(0.0);
    let (mut points, preserve_vertices) = match shape_kind {
        1 => parse_shape_line(flat, shape, origin)?,
        2 => {
            let mut relative = flat.point_vector(shape, 0)?;
            if relative.is_empty() {
                return None;
            }
            for point in &mut relative {
                point.0 += origin.0;
                point.1 += origin.1;
            }
            if relative.len() >= 3 && relative.first() != relative.last() {
                relative.push(relative[0]);
            }
            (relative, true)
        }
        3 => {
            let (width, height) = flat.pair_field(shape, 1)?;
            (
                ellipse_bezier_points(
                    (origin.0 + width * 0.5, origin.1 + height * 0.5),
                    width.abs() * 0.5,
                    height.abs() * 0.5,
                ),
                false,
            )
        }
        _ => return None,
    };
    if rotation.abs() > f32::EPSILON {
        rotate_points(&mut points, origin, rotation);
    }
    let tool = flat
        .field(payload, 6)
        .and_then(|field| flat.u8(field))
        .unwrap_or(0);
    let dash = flat
        .field(payload, 7)
        .and_then(|field| flat.u8(field))
        .unwrap_or(1);
    Some(StrokeCurve {
        page_index: page_index(flat, payload, 0),
        points,
        preserve_vertices,
        path_commands: Vec::new(),
        fill_path: false,
        width: flat.field(payload, 10).and_then(|field| flat.f32(field))?,
        rgba: flat
            .field(payload, 9)
            .and_then(|field| flat.bytes::<4>(field))
            .unwrap_or([0, 0, 0, 255]),
        style: tool_style(tool),
        dash_pattern: (dash > 1).then_some(dash - 1),
        pressures: vec![1.0; 1],
        fractional_widths: vec![1.0; 1],
        opacities: Vec::new(),
    })
}

fn parse_shape_line(
    flat: &FlatBuffer<'_>,
    shape: usize,
    origin: (f32, f32),
) -> Option<(Vec<(f32, f32)>, bool)> {
    let start_delta = flat.pair_field(shape, 0).unwrap_or((0.0, 0.0));
    let end_delta = flat.pair_field(shape, 3)?;
    let start = (origin.0 + start_delta.0, origin.1 + start_delta.1);
    let end = (origin.0 + end_delta.0, origin.1 + end_delta.1);
    let Some(control_delta) = flat.pair_field(shape, 1) else {
        return Some((vec![start, end], true));
    };
    let control = (origin.0 + control_delta.0, origin.1 + control_delta.1);
    let control1 = (
        start.0 + (control.0 - start.0) * (2.0 / 3.0),
        start.1 + (control.1 - start.1) * (2.0 / 3.0),
    );
    let control2 = (
        end.0 + (control.0 - end.0) * (2.0 / 3.0),
        end.1 + (control.1 - end.1) * (2.0 / 3.0),
    );
    Some((vec![start, control1, control2, end], false))
}

struct DecodedInk {
    points: Vec<(f32, f32)>,
    preserve_vertices: bool,
    pressures: Vec<f32>,
    fractional_widths: Vec<f32>,
    opacities: Vec<f32>,
}

fn decode_ink_path(data: &[u8], origin: (f32, f32), scale: (f32, f32)) -> Option<Vec<DecodedInk>> {
    if data.len() < 8 {
        return None;
    }
    let precision = data[0];
    let sample_count = u16::from_le_bytes(data.get(1..3)?.try_into().ok()?) as usize;
    let path_kind = data[3];
    if sample_count == 0 || !matches!(precision, 0 | 1) {
        return None;
    }
    let scalar_size = if precision == 0 { 2 } else { 4 };
    let read_scalar = |offset: usize| -> Option<f32> {
        if precision == 0 {
            Some(f16_to_f32(u16::from_le_bytes(
                data.get(offset..offset + 2)?.try_into().ok()?,
            )))
        } else {
            Some(f32::from_le_bytes(
                data.get(offset..offset + 4)?.try_into().ok()?,
            ))
        }
    };
    if path_kind == 7 {
        let mut cursor = if precision == 0 { 8 } else { 12 };
        let mut paths = Vec::new();
        let mut points = vec![origin];
        for _ in 1..sample_count {
            let command = *data.get(cursor)?;
            let x = read_scalar(cursor + 1)?;
            let y = read_scalar(cursor + 1 + scalar_size)?;
            let point = (origin.0 + x * scale.0, origin.1 + y * scale.1);
            match command {
                6 => points.push(point),
                7 => {
                    push_decoded_path(&mut paths, points, true, false);
                    points = vec![point];
                }
                _ => return None,
            }
            cursor += 1 + scalar_size * 2;
        }
        push_decoded_path(&mut paths, points, true, sample_count == 1);
        return Some(paths);
    }
    if path_kind != 3 {
        return None;
    }

    let mut cursor = if precision == 0 { 8 } else { 12 };
    let mut paths = Vec::new();
    let mut points = vec![origin];
    let mut pressures = Vec::new();
    let mut fractional_widths = Vec::new();
    let mut opacities = Vec::new();
    for _ in 1..sample_count {
        pressures.push(read_half(data, cursor).unwrap_or(1.0));
        fractional_widths.push(read_half(data, cursor + 2).unwrap_or(1.0));
        let command = *data.get(cursor + 6)?;
        let coordinate_count = match command {
            0 => 6,
            3 => 2,
            _ => return None,
        };
        let mut values = Vec::with_capacity(coordinate_count);
        for index in 0..coordinate_count {
            values.push(read_scalar(cursor + 7 + index * scalar_size)?);
        }
        match command {
            0 => {
                opacities.push(*data.get(cursor + 4)? as f32 / 255.0);
                points.extend([
                    (
                        origin.0 + values[0] * scale.0,
                        origin.1 + values[1] * scale.1,
                    ),
                    (
                        origin.0 + values[2] * scale.0,
                        origin.1 + values[3] * scale.1,
                    ),
                    (
                        origin.0 + values[4] * scale.0,
                        origin.1 + values[5] * scale.1,
                    ),
                ]);
            }
            3 => {
                push_decoded_path_with_widths(
                    &mut paths,
                    points,
                    pressures,
                    fractional_widths,
                    opacities,
                    false,
                );
                points = vec![(
                    origin.0 + values[0] * scale.0,
                    origin.1 + values[1] * scale.1,
                )];
                pressures = Vec::new();
                fractional_widths = Vec::new();
                opacities = Vec::new();
            }
            _ => unreachable!(),
        }
        cursor += 7 + coordinate_count * scalar_size;
    }
    pressures.push(read_half(data, cursor).unwrap_or(1.0));
    fractional_widths.push(read_half(data, cursor + 2).unwrap_or(1.0));
    if sample_count == 1 {
        if let Some(alpha) = data.get(cursor + 4) {
            opacities.push(*alpha as f32 / 255.0);
        }
    }
    push_decoded_path_with_widths(
        &mut paths,
        points,
        pressures,
        fractional_widths,
        opacities,
        sample_count == 1,
    );
    Some(paths)
}

fn push_decoded_path(
    paths: &mut Vec<DecodedInk>,
    points: Vec<(f32, f32)>,
    preserve_vertices: bool,
    keep_single_point: bool,
) {
    if points.len() < 2 && !keep_single_point {
        return;
    }
    paths.push(DecodedInk {
        pressures: vec![1.0; points.len()],
        fractional_widths: vec![1.0; points.len()],
        opacities: Vec::new(),
        points,
        preserve_vertices,
    });
}

fn push_decoded_path_with_widths(
    paths: &mut Vec<DecodedInk>,
    points: Vec<(f32, f32)>,
    pressures: Vec<f32>,
    fractional_widths: Vec<f32>,
    opacities: Vec<f32>,
    keep_single_point: bool,
) {
    if points.len() < 2 && !keep_single_point {
        return;
    }
    paths.push(DecodedInk {
        points,
        preserve_vertices: false,
        pressures,
        fractional_widths,
        opacities,
    });
}

fn read_half(data: &[u8], offset: usize) -> Option<f32> {
    Some(f16_to_f32(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    )))
}

fn f16_to_f32(value: u16) -> f32 {
    let sign = ((value & 0x8000) as u32) << 16;
    let exponent = ((value >> 10) & 0x1f) as u32;
    let fraction = (value & 0x03ff) as u32;
    let bits = match exponent {
        0 if fraction == 0 => sign,
        0 => {
            let shift = fraction.leading_zeros() - 21;
            let normalized = (fraction << shift) & 0x03ff;
            sign | ((127 - 15 - shift + 1) << 23) | (normalized << 13)
        }
        0x1f => sign | 0x7f80_0000 | (fraction << 13),
        _ => sign | ((exponent + 127 - 15) << 23) | (fraction << 13),
    };
    f32::from_bits(bits)
}

fn rotate_points(points: &mut [(f32, f32)], origin: (f32, f32), angle: f32) {
    let cos = angle.cos();
    let sin = angle.sin();
    for point in points {
        let x = point.0 - origin.0;
        let y = point.1 - origin.1;
        *point = (origin.0 + x * cos - y * sin, origin.1 + x * sin + y * cos);
    }
}

fn ellipse_bezier_points(center: (f32, f32), radius_x: f32, radius_y: f32) -> Vec<(f32, f32)> {
    let kappa = 0.552_284_8f32;
    let (cx, cy) = center;
    vec![
        (cx + radius_x, cy),
        (cx + radius_x, cy + radius_y * kappa),
        (cx + radius_x * kappa, cy + radius_y),
        (cx, cy + radius_y),
        (cx - radius_x * kappa, cy + radius_y),
        (cx - radius_x, cy + radius_y * kappa),
        (cx - radius_x, cy),
        (cx - radius_x, cy - radius_y * kappa),
        (cx - radius_x * kappa, cy - radius_y),
        (cx, cy - radius_y),
        (cx + radius_x * kappa, cy - radius_y),
        (cx + radius_x, cy - radius_y * kappa),
        (cx + radius_x, cy),
    ]
}

fn tool_style(value: u8) -> u8 {
    match value {
        1 => STROKE_STYLE_PENCIL,
        2 => STROKE_STYLE_HIGHLIGHTER,
        _ => 3,
    }
}

fn page_index(flat: &FlatBuffer<'_>, table: usize, field_index: usize) -> usize {
    flat.field(table, field_index)
        .and_then(|field| flat.u32(field + 8))
        .unwrap_or(0) as usize
}

fn position_index(flat: &FlatBuffer<'_>, table: usize, field_index: usize) -> usize {
    let Some(field) = flat.field(table, field_index) else {
        return 0;
    };
    let section = flat.u32(field + 4).unwrap_or(0);
    let index = flat.u32(field + 8).unwrap_or(0) as usize;
    if section >= 3 && index == 0 {
        usize::MAX
    } else {
        index
    }
}

fn nested_string<'a>(
    flat: &FlatBuffer<'a>,
    table: usize,
    outer: usize,
    inner: usize,
) -> Option<&'a str> {
    flat.offset_field(table, outer)
        .and_then(|value| flat.string_field(value, inner))
}

fn nested_f32(flat: &FlatBuffer<'_>, table: usize, outer: usize, inner: usize) -> Option<f32> {
    flat.offset_field(table, outer)
        .and_then(|value| flat.field(value, inner))
        .and_then(|field| flat.f32(field))
}

fn nested_u8(flat: &FlatBuffer<'_>, table: usize, outer: usize, inner: usize) -> Option<u8> {
    flat.offset_field(table, outer)
        .and_then(|value| flat.field(value, inner))
        .and_then(|field| flat.u8(field))
}

fn nested_bool(flat: &FlatBuffer<'_>, table: usize, outer: usize, inner: usize) -> Option<bool> {
    flat.offset_field(table, outer).map(|value| {
        flat.field(value, inner)
            .and_then(|field| flat.u8(field))
            .unwrap_or(0)
            != 0
    })
}

fn default_text_style() -> TextStyle {
    TextStyle {
        font_size: 12.0,
        font_name: "Helvetica".to_owned(),
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

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

struct FlatBuffer<'a> {
    data: &'a [u8],
}

impl<'a> FlatBuffer<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn root_table(&self) -> Option<usize> {
        Some(self.u32(0)? as usize)
    }

    fn field(&self, table: usize, index: usize) -> Option<usize> {
        let delta = self.i32(table)? as i64;
        let vtable = (table as i64).checked_sub(delta)?;
        if vtable < 0 {
            return None;
        }
        let vtable = vtable as usize;
        let length = self.u16(vtable)? as usize;
        let entry = 4usize.checked_add(index.checked_mul(2)?)?;
        if entry + 2 > length {
            return None;
        }
        let offset = self.u16(vtable + entry)? as usize;
        (offset != 0).then_some(table.checked_add(offset)?)
    }

    fn offset_field(&self, table: usize, index: usize) -> Option<usize> {
        let field = self.field(table, index)?;
        field.checked_add(self.u32(field)? as usize)
    }

    fn string_field(&self, table: usize, index: usize) -> Option<&'a str> {
        let target = self.offset_field(table, index)?;
        let length = self.u32(target)? as usize;
        std::str::from_utf8(self.slice(target + 4, length)?).ok()
    }

    fn byte_vector(&self, table: usize, index: usize) -> Option<&'a [u8]> {
        let target = self.offset_field(table, index)?;
        let length = self.u32(target)? as usize;
        self.slice(target + 4, length)
    }

    fn point_vector(&self, table: usize, index: usize) -> Option<Vec<(f32, f32)>> {
        let target = self.offset_field(table, index)?;
        let length = self.u32(target)? as usize;
        (0..length)
            .map(|item| {
                let position = target + 4 + item * 8;
                Some((self.f32(position)?, self.f32(position + 4)?))
            })
            .collect()
    }

    fn table_vector(&self, table: usize, index: usize) -> Option<Vec<usize>> {
        let target = self.offset_field(table, index)?;
        let length = self.u32(target)? as usize;
        (0..length)
            .map(|item| {
                let slot = target + 4 + item * 4;
                slot.checked_add(self.u32(slot)? as usize)
            })
            .collect()
    }

    fn pair_field(&self, table: usize, index: usize) -> Option<(f32, f32)> {
        let field = self.field(table, index)?;
        Some((self.f32(field)?, self.f32(field + 4)?))
    }

    fn float_array<const N: usize>(&self, position: usize) -> Option<[f32; N]> {
        let mut values = [0.0; N];
        for (index, value) in values.iter_mut().enumerate() {
            *value = self.f32(position + index * 4)?;
        }
        Some(values)
    }

    fn bytes<const N: usize>(&self, position: usize) -> Option<[u8; N]> {
        self.slice(position, N)?.try_into().ok()
    }

    fn slice(&self, position: usize, length: usize) -> Option<&'a [u8]> {
        self.data.get(position..position.checked_add(length)?)
    }

    fn u8(&self, position: usize) -> Option<u8> {
        self.data.get(position).copied()
    }

    fn u16(&self, position: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(position)?))
    }

    fn u32(&self, position: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(position)?))
    }

    fn i32(&self, position: usize) -> Option<i32> {
        Some(i32::from_le_bytes(self.bytes(position)?))
    }

    fn u64(&self, position: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(position)?))
    }

    fn f32(&self, position: usize) -> Option<f32> {
        Some(f32::from_le_bytes(self.bytes(position)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_ink_path, f16_to_f32, rotate_points, unrotate_top_left};

    #[test]
    fn decodes_half_precision_values() {
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
    }

    #[test]
    fn recovers_unrotated_media_origin() {
        let origin = unrotate_top_left((555.241, 614.90533), (115.95187, 154.6025), 0.927594);
        assert!((origin.0 - 470.18194).abs() < 0.0001);
        assert!((origin.1 - 630.3575).abs() < 0.0001);
    }

    #[test]
    fn applies_ink_scale_to_relative_coordinates() {
        let encoded = [0, 2, 0, 7, 0, 0, 0, 0, 6, 0x00, 0x3c, 0x00, 0x40];
        let decoded = decode_ink_path(&encoded, (10.0, 20.0), (0.5, 2.0)).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].points, vec![(10.0, 20.0), (10.5, 24.0)]);
    }

    #[test]
    fn rotates_ink_around_its_origin() {
        let mut points = vec![(11.0, 20.0)];
        rotate_points(&mut points, (10.0, 20.0), std::f32::consts::FRAC_PI_2);
        assert!((points[0].0 - 10.0).abs() < 0.0001);
        assert!((points[0].1 - 21.0).abs() < 0.0001);
    }

    #[test]
    fn splits_linear_ink_at_move_commands() {
        let encoded = [
            0, 4, 0, 7, 0, 0, 0, 0, // half precision, four samples, linear path
            6, 0x00, 0x3c, 0, 0, // line to (1, 0)
            7, 0x00, 0x49, 0, 0, // move to (10, 0)
            6, 0x00, 0x4a, 0, 0, // line to (12, 0)
        ];
        let decoded = decode_ink_path(&encoded, (0.0, 0.0), (1.0, 1.0)).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].points, vec![(0.0, 0.0), (1.0, 0.0)]);
        assert_eq!(decoded[1].points, vec![(10.0, 0.0), (12.0, 0.0)]);
    }

    #[test]
    fn splits_cubic_ink_at_move_commands() {
        let mut encoded = vec![0, 4, 0, 3, 0, 0, 0, 0];
        let mut push_sample = |command: u8, coordinates: &[(u16, u16)]| {
            encoded.extend_from_slice(&[0x00, 0x3c, 0x00, 0x3c, 0, 0, command]);
            for (x, y) in coordinates {
                encoded.extend_from_slice(&x.to_le_bytes());
                encoded.extend_from_slice(&y.to_le_bytes());
            }
        };
        push_sample(0, &[(0x3c00, 0), (0x4000, 0), (0x4200, 0)]);
        push_sample(3, &[(0x4900, 0)]);
        push_sample(0, &[(0x4980, 0), (0x4a00, 0), (0x4a80, 0)]);
        encoded.extend_from_slice(&[0x00, 0x3c, 0x00, 0x3c, 0, 0]);

        let decoded = decode_ink_path(&encoded, (0.0, 0.0), (1.0, 1.0)).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(
            decoded[0].points,
            vec![(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 0.0)]
        );
        assert_eq!(
            decoded[1].points,
            vec![(10.0, 0.0), (11.0, 0.0), (12.0, 0.0), (13.0, 0.0)]
        );
    }

    #[test]
    fn keeps_segment_opacity_with_its_erased_fragment() {
        let mut encoded = vec![0, 4, 0, 3, 0, 0, 0, 0];
        for (command, alpha, coordinates) in [
            (0u8, 128u8, vec![0x3c00u16, 0, 0x4000, 0, 0x4200, 0]),
            (3, 255, vec![0x4900, 0]),
            (0, 64, vec![0x4980, 0, 0x4a00, 0, 0x4a80, 0]),
        ] {
            encoded.extend([0, 0x3c, 0, 0x3c, alpha, 0, command]);
            for value in coordinates {
                encoded.extend(value.to_le_bytes());
            }
        }
        encoded.extend([0, 0x3c, 0, 0x3c, 255, 0]);
        let decoded = decode_ink_path(&encoded, (0.0, 0.0), (1.0, 1.0)).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].opacities, vec![128.0 / 255.0]);
        assert_eq!(decoded[1].opacities, vec![64.0 / 255.0]);
        assert_eq!(decoded[0].pressures.len(), 2);
        assert_eq!(decoded[1].pressures.len(), 2);
    }
}
