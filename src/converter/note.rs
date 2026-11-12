use super::archive::KeyedArchive;
use super::model::{
    EmbeddedPdfPage, ImageCrop, MediaImage, NoteDocument, StickyNote, StrokeCurve, TextBlock,
    TextSpan, TextStyle, DEFAULT_PAGE_RATIO,
};
use super::util::{choose_export_width, parse_pair, parse_rect, value_f32, value_i64};
use crate::pdf::{page_display_size, page_id_by_index};
use crate::Result;
use image::GenericImageView;
use lopdf::Document;
use plist::{Dictionary, Value};
use std::collections::BTreeMap;
#[cfg(not(target_arch = "wasm32"))]
use std::fs::File;
use std::io::{Cursor, Read, Seek};
use std::path::Path;
use zip::ZipArchive;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn load_note_document(note_path: &Path) -> Result<NoteDocument> {
    read_note_document(ZipArchive::new(File::open(note_path)?)?)
}

pub(crate) fn read_note_document<R: Read + Seek>(
    mut bundle: ZipArchive<R>,
) -> Result<NoteDocument> {
    let entries: Vec<String> = bundle.file_names().map(str::to_owned).collect();
    let bundle_root = entries
        .iter()
        .find_map(|entry| entry.strip_suffix("Session.plist").map(str::to_owned))
        .ok_or("Could not find Session.plist inside .note bundle")?;
    let session_bytes = read_zip_entry(&mut bundle, &(bundle_root.clone() + "Session.plist"))?;
    let archive = KeyedArchive::new(Value::from_reader(Cursor::new(session_bytes))?)?;
    let session = archive
        .root
        .as_dictionary()
        .ok_or("Unexpected Session.plist root object")?;
    let rich_text_value = archive.deref(session.get("richText").ok_or("Session has no richText")?);
    let rich_text = rich_text_value
        .as_dictionary()
        .ok_or("Unexpected richText object")?;
    let reflow_state_value = archive.deref(
        rich_text
            .get("reflowState")
            .ok_or("richText has no reflowState")?,
    );
    let reflow_state = reflow_state_value
        .as_dictionary()
        .ok_or("Unexpected reflowState object")?;
    let page_width_doc = reflow_state
        .get("pageWidthInDocumentCoordsKey")
        .and_then(value_f32)
        .unwrap_or(679.0);

    let (line_style, paper_size, sizing_behavior) = parse_paper_attributes(&archive, session);
    let layout_width_pt = choose_export_width(paper_size.as_deref());
    let export_width_pt =
        locked_export_width(sizing_behavior.as_deref()).unwrap_or(layout_width_pt);
    let pdf_pages = parse_pdf_pages(&archive, rich_text);
    let page_ratio = if sizing_behavior.as_deref() == Some("staticWidth")
        && paper_size
            .as_deref()
            .is_some_and(|size| size.eq_ignore_ascii_case("letter"))
    {
        (page_width_doc * 11.0 / 8.5).floor() / page_width_doc
    } else if let Some(first_pdf) = pdf_pages.first() {
        pdf_page_ratio(&mut bundle, &bundle_root, first_pdf).unwrap_or(DEFAULT_PAGE_RATIO)
    } else if let Some(thumb) = choose_thumbnail_entry(&entries, &bundle_root) {
        thumbnail_ratio(&mut bundle, &thumb).unwrap_or(DEFAULT_PAGE_RATIO)
    } else {
        DEFAULT_PAGE_RATIO
    };
    let export_height_pt = export_width_pt * page_ratio;
    let page_height_doc = page_width_doc * page_ratio;

    let (text, text_spans) = parse_text(&archive, rich_text);
    let text_blocks = parse_text_blocks(&archive, rich_text, page_height_doc);
    let media_images = parse_media_images(&archive, rich_text, page_height_doc);
    let sticky_notes = parse_sticky_notes(&archive, rich_text, page_height_doc);
    let curves = parse_curves(&archive, rich_text, page_height_doc);
    Ok(NoteDocument {
        bundle_root,
        page_width_doc,
        page_height_doc,
        export_width_pt,
        export_height_pt,
        layout_width_pt,
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

fn locked_export_width(sizing_behavior: Option<&str>) -> Option<f32> {
    let behavior = sizing_behavior?;
    let mut parts = behavior.split(':');
    if parts.next()? != "lockedWidth" {
        return None;
    }
    let width = parts.next()?.parse::<f32>().ok()?;
    width
        .is_finite()
        .then_some(width)
        .filter(|width| *width > 0.0)
}

pub(crate) fn read_zip_entry<R: Read + Seek>(
    bundle: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>> {
    let mut file = bundle.by_name(name)?;
    let mut data = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut data)?;
    Ok(data)
}

fn parse_paper_attributes(
    archive: &KeyedArchive,
    session: &Dictionary,
) -> (Option<String>, Option<String>, Option<String>) {
    let paper_model = session
        .get("NBNoteTakingSessionDocumentPaperLayoutModelKey")
        .map(|value| archive.deref(value));
    let paper_model = paper_model.and_then(Value::as_dictionary);
    let attrs = paper_model
        .and_then(|model| model.get("documentPaperAttributes"))
        .map(|value| archive.deref(value))
        .and_then(Value::as_dictionary);
    let Some(attrs) = attrs else {
        return (None, None, None);
    };
    let line_style = attrs
        .get("lineStyle2")
        .and_then(|value| archive.as_text(value).ok());
    let paper_size = attrs
        .get("paperSize")
        .and_then(|value| archive.as_text(value).ok());
    let sizing = attrs
        .get("paperSizingBehavior")
        .and_then(|value| archive.as_text(value).ok());
    (line_style, paper_size, sizing)
}

fn choose_thumbnail_entry(entries: &[String], bundle_root: &str) -> Option<String> {
    entries
        .iter()
        .filter(|entry| {
            entry.starts_with(bundle_root)
                && entry.to_ascii_lowercase().ends_with(".png")
                && Path::new(entry)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.to_ascii_lowercase().contains("thumb"))
        })
        .max_by_key(|entry| entry.matches('x').count())
        .cloned()
}

fn thumbnail_ratio<R: Read + Seek>(bundle: &mut ZipArchive<R>, entry: &str) -> Result<f32> {
    let data = read_zip_entry(bundle, entry)?;
    let image = image::load_from_memory(&data)?;
    let (width, height) = image.dimensions();
    Ok(height as f32 / width as f32)
}

fn pdf_page_ratio<R: Read + Seek>(
    bundle: &mut ZipArchive<R>,
    bundle_root: &str,
    page: &EmbeddedPdfPage,
) -> Result<f32> {
    let data = read_zip_entry(bundle, &(bundle_root.to_owned() + &page.relative_path))?;
    let doc = Document::load_mem(&data)?;
    let page_id = page_id_by_index(&doc, page.source_page_index)?;
    let (width, height) = page_display_size(&doc, page_id)?;
    Ok(height / width)
}

fn parse_text(archive: &KeyedArchive, rich_text: &Dictionary) -> (String, Vec<TextSpan>) {
    let attributed = archive.ns_dict(rich_text.get("attributedString"));
    let text = attributed
        .get("stringKey")
        .and_then(|value| archive.as_text(value).ok())
        .unwrap_or_default();
    // Apple string ranges count UTF-16 units, while renderer spans count characters.
    let mut characters_by_unit = Vec::with_capacity(text.len() + 1);
    for (index, ch) in text.chars().enumerate() {
        characters_by_unit.extend(std::iter::repeat_n(index, ch.len_utf16()));
    }
    characters_by_unit.push(text.chars().count());
    let character_offset =
        |unit: f32| characters_by_unit[(unit.max(0.0) as usize).min(characters_by_unit.len() - 1)];
    let mut spans = Vec::new();
    for raw_range in archive.ns_array(attributed.get("subRangesKey")) {
        let subrange = archive.ns_dict(Some(&raw_range));
        let Some(range_text) = subrange
            .get("subRangeRangeKey")
            .and_then(|value| archive.as_text(value).ok())
        else {
            continue;
        };
        let (start, length) = parse_pair(&range_text).unwrap_or((0.0, text.len() as f32));
        let font_attrs = archive.ns_dict(subrange.get("subRangeFontKey"));
        let font_size = font_attrs
            .get("NSFontSizeAttribute")
            .and_then(value_f32)
            .unwrap_or(12.0);
        let font_name = font_attrs
            .get("NSFontNameAttribute")
            .and_then(|value| archive.as_text(value).ok())
            .unwrap_or_else(|| "Helvetica".to_string());
        let other_attrs = archive.ns_dict(subrange.get("subRangeOtherAttributesKey"));
        let line_spacing_multiplier = other_attrs
            .get("line-spacing")
            .and_then(value_f32)
            .unwrap_or(1.0);
        let color = subrange
            .get("subRangeColorCrossPlatformKey")
            .or_else(|| subrange.get("subRangeColorKey"))
            .map(|value| parse_text_color(archive.deref(value)))
            .unwrap_or([0, 0, 0, 255]);
        let underline = other_attrs
            .get("NSUnderline")
            .and_then(value_i64)
            .unwrap_or(0)
            != 0;
        let strikethrough = other_attrs
            .get("NSStrikethrough")
            .and_then(value_i64)
            .unwrap_or(0)
            != 0;
        let baseline_offset = other_attrs
            .get("NSBaselineOffset")
            .and_then(value_f32)
            .unwrap_or(0.0);
        let indent_level = other_attrs
            .get("indent-level")
            .and_then(value_i64)
            .map(|level| level.max(0) as usize);
        let indent_decoration_style = other_attrs
            .get("indent-decoration-style")
            .and_then(value_i64);
        let indent_decoration_number = other_attrs
            .get("indent-decoration-number")
            .and_then(value_i64);
        let checklist_checked = other_attrs
            .get("checklist-checked")
            .and_then(value_i64)
            .unwrap_or(0)
            != 0;
        let lower_font_name = font_name.to_ascii_lowercase();
        spans.push(TextSpan {
            start: character_offset(start),
            length: character_offset(start + length).saturating_sub(character_offset(start)),
            style: TextStyle {
                font_size,
                font_name,
                line_spacing_multiplier,
                color,
                bold: lower_font_name.contains("bold"),
                italic: lower_font_name.contains("italic") || lower_font_name.contains("oblique"),
                underline,
                strikethrough,
                baseline_offset,
                indent_level,
                indent_decoration_style,
                indent_decoration_number,
                checklist_checked,
            },
        });
    }
    if spans.is_empty() {
        spans.push(TextSpan {
            start: 0,
            length: text.chars().count(),
            style: TextStyle {
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
            },
        });
    }
    (text, spans)
}

fn parse_text_color(value: &Value) -> [u8; 4] {
    if let Some(text) = value.as_string() {
        let parts: Vec<f32> = text
            .split(',')
            .filter_map(|part| part.parse::<f32>().ok())
            .collect();
        if parts.len() == 4 {
            return [
                unit_to_u8(parts[0]),
                unit_to_u8(parts[1]),
                unit_to_u8(parts[2]),
                unit_to_u8(parts[3]),
            ];
        }
    }
    if let Some(dict) = value.as_dictionary() {
        let white = dict.get("UIWhite").and_then(value_f32).unwrap_or(0.0);
        return [
            unit_to_u8(dict.get("UIRed").and_then(value_f32).unwrap_or(white)),
            unit_to_u8(dict.get("UIGreen").and_then(value_f32).unwrap_or(white)),
            unit_to_u8(dict.get("UIBlue").and_then(value_f32).unwrap_or(white)),
            unit_to_u8(dict.get("UIAlpha").and_then(value_f32).unwrap_or(1.0)),
        ];
    }
    [0, 0, 0, 255]
}

fn unit_to_u8(value: f32) -> u8 {
    (value * 255.0).round().clamp(0.0, 255.0) as u8
}

fn parse_text_blocks(
    archive: &KeyedArchive,
    rich_text: &Dictionary,
    page_height_doc: f32,
) -> Vec<TextBlock> {
    let mut blocks = Vec::new();
    for raw_media in archive.ns_array(rich_text.get("mediaObjects")) {
        let Some(media) = raw_media.as_dictionary() else {
            continue;
        };
        let class_name = media
            .get("$class")
            .map(|value| archive.deref(value))
            .and_then(Value::as_dictionary)
            .and_then(|class| class.get("$classname"))
            .and_then(Value::as_string)
            .unwrap_or("");
        if class_name != "TextBlockMediaObject" {
            continue;
        }
        let Some((x, y)) = media
            .get("documentOrigin")
            .and_then(|value| archive.as_text(value).ok())
            .and_then(|text| parse_pair(&text))
        else {
            continue;
        };
        let Some((width, height)) = media
            .get("unscaledContentSize")
            .and_then(|value| archive.as_text(value).ok())
            .and_then(|text| parse_pair(&text))
        else {
            continue;
        };
        let Some(text_store) = media
            .get("textStore")
            .map(|value| archive.deref(value))
            .and_then(Value::as_dictionary)
        else {
            continue;
        };
        let (text, text_spans) = parse_text(archive, text_store);
        let page_index = (y / page_height_doc).floor() as usize;
        blocks.push(TextBlock {
            page_index,
            x,
            y: y - page_index as f32 * page_height_doc,
            width,
            height,
            text,
            rotation_degrees: media
                .get("rotationDegrees")
                .and_then(value_f32)
                .map(normalize_rotation_degrees)
                .unwrap_or(0.0),
            text_spans,
            z_index: media.get("zIndex").and_then(value_i64).unwrap_or(0),
        });
    }
    blocks.sort_by_key(|block| (block.page_index, block.z_index));
    blocks
}

fn parse_media_images(
    archive: &KeyedArchive,
    rich_text: &Dictionary,
    page_height_doc: f32,
) -> Vec<MediaImage> {
    let mut images = Vec::new();
    for raw_media in archive.ns_array(rich_text.get("mediaObjects")) {
        let Some(media) = raw_media.as_dictionary() else {
            continue;
        };
        let Some((x, y)) = media
            .get("documentOrigin")
            .and_then(|value| archive.as_text(value).ok())
            .and_then(|text| parse_pair(&text))
        else {
            continue;
        };
        let Some((width, height)) = media
            .get("unscaledContentSize")
            .and_then(|value| archive.as_text(value).ok())
            .and_then(|text| parse_pair(&text))
        else {
            continue;
        };
        let figure = media
            .get("figure")
            .map(|value| archive.deref(value))
            .and_then(Value::as_dictionary);
        let relative_path = figure
            .and_then(|figure| figure.get("FigureBackgroundObjectKey"))
            .map(|value| archive.deref(value))
            .and_then(Value::as_dictionary)
            .and_then(|background| background.get("kImageObjectSnapshotKey"))
            .map(|value| archive.deref(value))
            .and_then(Value::as_dictionary)
            .and_then(|snapshot| snapshot.get("relativePath"))
            .and_then(|value| archive.as_text(value).ok());
        let Some(relative_path) = relative_path else {
            continue;
        };
        let crop = figure
            .and_then(|figure| figure.get("FigureCropRectKey"))
            .and_then(|value| archive.as_text(value).ok())
            .and_then(|text| parse_rect(&text))
            .and_then(|(x, y, width, height)| {
                (width > 0.0 && height > 0.0).then_some(ImageCrop {
                    x,
                    y,
                    width,
                    height,
                    reference_size: None,
                })
            });
        let page_index = (y / page_height_doc).floor() as usize;
        images.push(MediaImage {
            page_index,
            x,
            y: y - page_index as f32 * page_height_doc,
            width,
            height,
            rotation_degrees: media
                .get("rotationDegrees")
                .and_then(value_f32)
                .map(normalize_rotation_degrees)
                .unwrap_or(0.0),
            // Saved image snapshots are used as supplied by the official export.
            // These legacy editor flags are not extra transforms on the snapshot.
            flipped_horizontal: false,
            flipped_vertical: false,
            relative_path,
            z_index: media.get("zIndex").and_then(value_i64).unwrap_or(0),
            crop,
        });
    }
    images.sort_by_key(|image| (image.page_index, image.z_index));
    images
}

fn parse_sticky_notes(
    archive: &KeyedArchive,
    rich_text: &Dictionary,
    page_height_doc: f32,
) -> Vec<StickyNote> {
    let mut sticky_notes = Vec::new();
    for raw_media in archive.ns_array(rich_text.get("mediaObjects")) {
        let Some(media) = raw_media.as_dictionary() else {
            continue;
        };
        let paper_style = media
            .get("paperStyleObject")
            .map(|value| archive.deref(value))
            .and_then(Value::as_dictionary);
        let paper_attrs = media
            .get("kCanvasMediaObjectPaperAttributes")
            .map(|value| archive.deref(value))
            .and_then(Value::as_dictionary);
        if paper_style.is_none() && paper_attrs.is_none() {
            continue;
        }
        // Plain text boxes share canvas metadata with sticky notes, but have no paper.
        let has_color = paper_style
            .and_then(|style| style.get("paperColor"))
            .map(|value| archive.deref(value))
            .is_some_and(|value| {
                value.as_dictionary().is_some()
                    || value.as_string().is_some_and(|text| text != "$null")
            });
        if media.contains_key("textStore") && !has_color {
            continue;
        }
        let Some((x, y)) = media
            .get("documentOrigin")
            .and_then(|value| archive.as_text(value).ok())
            .and_then(|text| parse_pair(&text))
        else {
            continue;
        };
        let Some((width, height)) = media
            .get("unscaledContentSize")
            .and_then(|value| archive.as_text(value).ok())
            .and_then(|text| parse_pair(&text))
        else {
            continue;
        };
        let page_index = (y / page_height_doc).floor() as usize;
        let color = paper_style
            .and_then(|style| style.get("paperColor"))
            .map(|value| parse_text_color(archive.deref(value)))
            .unwrap_or([255, 244, 142, 255]);
        let line_style = paper_attrs
            .and_then(|attrs| attrs.get("lineStyle2"))
            .and_then(|value| archive.as_text(value).ok());
        sticky_notes.push(StickyNote {
            page_index,
            x,
            y: y - page_index as f32 * page_height_doc,
            width,
            height,
            rotation_degrees: media
                .get("rotationDegrees")
                .and_then(value_f32)
                .map(normalize_rotation_degrees)
                .unwrap_or(0.0),
            color,
            line_style,
            line_start_multiple: 1.5,
            z_index: media.get("zIndex").and_then(value_i64).unwrap_or(0),
        });
    }
    sticky_notes.sort_by_key(|sticky| (sticky.page_index, sticky.z_index));
    sticky_notes
}

fn normalize_rotation_degrees(raw: f32) -> f32 {
    if raw.abs() <= std::f32::consts::TAU + 0.001 {
        raw.to_degrees()
    } else {
        raw
    }
}

fn parse_pdf_pages(archive: &KeyedArchive, rich_text: &Dictionary) -> Vec<EmbeddedPdfPage> {
    let mut pages = Vec::new();
    for raw_layout in archive.ns_array(rich_text.get("pageLayoutArray")) {
        let layout = archive.ns_dict(Some(&raw_layout));
        let Some(document_page) = layout
            .get("kPageLayoutDocumentPageNumberKey")
            .and_then(value_i64)
        else {
            continue;
        };
        let Some(source_page) = layout
            .get("kPageLayoutPDFPageNumberKey")
            .and_then(value_i64)
        else {
            continue;
        };
        let Some(filename) = layout
            .get("kPageLayoutPDFFileNameKey")
            .and_then(|value| archive.as_text(value).ok())
        else {
            continue;
        };
        pages.push(EmbeddedPdfPage {
            page_index: document_page.saturating_sub(1) as usize,
            relative_path: format!("PDFs/{filename}"),
            source_page_index: source_page.saturating_sub(1) as usize,
        });
    }
    pages.sort_by_key(|page| page.page_index);
    pages
}

fn parse_curves(
    archive: &KeyedArchive,
    rich_text: &Dictionary,
    page_height_doc: f32,
) -> Vec<StrokeCurve> {
    let Some(spatial_hash) = rich_text
        .get("Handwriting Overlay")
        .map(|value| archive.deref(value))
        .and_then(Value::as_dictionary)
        .and_then(|overlay| overlay.get("SpatialHash"))
        .map(|value| archive.deref(value))
        .and_then(Value::as_dictionary)
    else {
        return Vec::new();
    };
    let mut curves =
        parse_curves_from_spatial_hash(Some(archive), spatial_hash, page_height_doc, None);
    curves.extend(parse_group_curves(archive, spatial_hash, page_height_doc));
    curves.extend(parse_shape_curves(archive, spatial_hash, page_height_doc));
    curves
}

fn parse_curves_from_spatial_hash(
    archive: Option<&KeyedArchive>,
    spatial_hash: &Dictionary,
    page_height_doc: f32,
    transform: Option<[f32; 6]>,
) -> Vec<StrokeCurve> {
    let counts = unpack_i32(archive, spatial_hash.get("curvesnumpoints"));
    let widths = unpack_f32(archive, spatial_hash.get("curveswidth"));
    let points_blob = unpack_f32(archive, spatial_hash.get("curvespoints"));
    let pressures_blob = unpack_f32(archive, spatial_hash.get("curvesforces"));
    let fractional_widths_blob = unpack_f32(archive, spatial_hash.get("curvesfractionalwidths"));
    let styles = value_data(archive, spatial_hash.get("curvesstyles")).unwrap_or(&[]);
    let dash_patterns = parse_dash_patterns(archive, spatial_hash.get("dashStyles"));
    let colors = value_data(archive, spatial_hash.get("curvescolors")).unwrap_or(&[]);
    let uuids = value_data(archive, spatial_hash.get("curveUUIDs")).unwrap_or(&[]);
    let stroke_numbers = stroke_numbers(uuids, counts.len(), styles.len());
    let sample_counts: Vec<usize> = counts
        .iter()
        .map(|count| bezier_sample_count(*count as usize))
        .collect();
    let use_bezier_samples = pressures_blob.len() == sample_counts.iter().sum::<usize>()
        && fractional_widths_blob.len() == sample_counts.iter().sum::<usize>();
    let mut pressure_index = 0usize;
    let mut fractional_index = 0usize;
    let mut points_index = 0usize;
    let mut curves = Vec::new();
    for (curve_index, point_count) in counts.iter().enumerate() {
        let mut points = Vec::with_capacity(*point_count as usize);
        for _ in 0..*point_count {
            if points_index + 1 >= points_blob.len() {
                break;
            }
            let point = (points_blob[points_index], points_blob[points_index + 1]);
            points.push(apply_transform(point, transform));
            points_index += 2;
        }
        let width =
            widths.get(curve_index).copied().unwrap_or(1.0) * transform_stroke_scale(transform);
        let stroke = stroke_numbers[curve_index];
        let style = styles.get(stroke).copied().unwrap_or(3);
        let dash_pattern = dash_patterns.get(&stroke).copied();
        let rgba = parse_handwriting_color(
            colors
                .get(curve_index * 4..curve_index * 4 + 4)
                .unwrap_or(&[]),
        );
        let (pressures, fractional_widths) = if use_bezier_samples {
            let sample_count = sample_counts[curve_index];
            let pressures = pressures_blob
                .get(pressure_index..pressure_index + sample_count)
                .unwrap_or(&[])
                .to_vec();
            let fractional_widths = fractional_widths_blob
                .get(fractional_index..fractional_index + sample_count)
                .unwrap_or(&[])
                .to_vec();
            pressure_index += sample_count;
            fractional_index += sample_count;
            (
                default_if_empty(pressures, sample_count),
                default_if_empty(fractional_widths, sample_count),
            )
        } else {
            (vec![1.0; points.len()], vec![1.0; points.len()])
        };
        curves.extend(split_curve_into_pages(
            points,
            false,
            width,
            rgba,
            style,
            dash_pattern,
            pressures,
            fractional_widths,
            page_height_doc,
        ));
    }
    curves
}

// Erasing can split one stroke into several curves with the same UUID. Some
// bundles keep styles per original stroke, others keep one style per curve.
fn stroke_numbers(uuids: &[u8], curve_count: usize, style_count: usize) -> Vec<usize> {
    if uuids.len() == curve_count.saturating_mul(16) && style_count != curve_count {
        let mut stroke = 0;
        let numbers: Vec<usize> = uuids
            .chunks_exact(16)
            .enumerate()
            .map(|(index, id)| {
                if index > 0 && id != &uuids[(index - 1) * 16..index * 16] {
                    stroke += 1;
                }
                stroke
            })
            .collect();
        // Only use stroke numbering when the style array confirms that encoding.
        if numbers.last().is_some_and(|last| last + 1 == style_count) {
            return numbers;
        }
    }
    (0..curve_count).collect()
}

fn parse_group_curves(
    archive: &KeyedArchive,
    spatial_hash: &Dictionary,
    page_height_doc: f32,
) -> Vec<StrokeCurve> {
    let mut curves = Vec::new();
    for raw_group in archive.ns_array(spatial_hash.get("groupsArrays")) {
        let Some(group_bytes) = raw_group.as_data() else {
            continue;
        };
        let Ok(group_value) = Value::from_reader(Cursor::new(group_bytes)) else {
            continue;
        };
        let Some(group) = group_value.as_dictionary() else {
            continue;
        };
        let Some(ink_group) = group.get("inkGroup").and_then(Value::as_dictionary) else {
            continue;
        };
        let transform = ink_group.get("transform").and_then(parse_transform);
        let Some(objects) = ink_group.get("inkGroupObjects").and_then(Value::as_array) else {
            continue;
        };
        for object in objects {
            let Some(entry) = object.as_dictionary() else {
                continue;
            };
            if let Some(object_bytes) = entry.get("object").and_then(Value::as_data) {
                let Ok(nested_value) = Value::from_reader(Cursor::new(object_bytes)) else {
                    continue;
                };
                let Ok(nested_archive) = KeyedArchive::new(nested_value) else {
                    continue;
                };
                let Some(nested_spatial_hash) = nested_archive.root.as_dictionary() else {
                    continue;
                };
                curves.extend(parse_curves_from_spatial_hash(
                    Some(&nested_archive),
                    nested_spatial_hash,
                    page_height_doc,
                    transform,
                ));
                continue;
            }
            if let Some(shape_root) = entry.get("object").and_then(Value::as_dictionary) {
                curves.extend(parse_shape_curves_from_root(
                    shape_root,
                    page_height_doc,
                    transform,
                ));
            }
        }
    }
    curves
}

fn parse_dash_patterns(
    archive: Option<&KeyedArchive>,
    value: Option<&Value>,
) -> BTreeMap<usize, u8> {
    let Some(data) = value_data(archive, value) else {
        return BTreeMap::new();
    };
    let Ok(value) = Value::from_reader(Cursor::new(data)) else {
        return BTreeMap::new();
    };
    let Some(patterns) = value
        .as_dictionary()
        .and_then(|root| root.get("objectPatterns"))
        .and_then(Value::as_dictionary)
    else {
        return BTreeMap::new();
    };
    patterns
        .iter()
        .filter_map(|(key, value)| {
            let index = key.parse::<usize>().ok()?;
            let pattern = value
                .as_dictionary()
                .and_then(|dict| dict.get("pattern"))
                .and_then(value_i64)? as u8;
            Some((index, pattern))
        })
        .collect()
}

fn parse_shape_curves(
    archive: &KeyedArchive,
    spatial_hash: &Dictionary,
    page_height_doc: f32,
) -> Vec<StrokeCurve> {
    let Some(shape_bytes) = spatial_hash
        .get("shapes")
        .map(|value| archive.deref(value))
        .and_then(Value::as_data)
    else {
        return Vec::new();
    };
    let Ok(value) = Value::from_reader(Cursor::new(shape_bytes)) else {
        return Vec::new();
    };
    let Some(root) = value.as_dictionary() else {
        return Vec::new();
    };
    parse_shape_curves_from_root(root, page_height_doc, None)
}

fn parse_shape_curves_from_root(
    root: &Dictionary,
    page_height_doc: f32,
    transform: Option<[f32; 6]>,
) -> Vec<StrokeCurve> {
    let Some(shapes) = root.get("shapes").and_then(Value::as_array) else {
        return Vec::new();
    };
    let kinds: Vec<&str> = root
        .get("kinds")
        .and_then(Value::as_array)
        .map(|kinds| kinds.iter().filter_map(Value::as_string).collect())
        .unwrap_or_default();
    let mut curves = Vec::new();
    for (index, shape) in shapes.iter().enumerate() {
        let Some(shape) = shape.as_dictionary() else {
            continue;
        };
        let kind = kinds.get(index).copied().unwrap_or("");
        let appearance = shape.get("appearance").and_then(Value::as_dictionary);
        let stroke_width = appearance
            .and_then(|appearance| appearance.get("strokeWidth"))
            .and_then(value_f32)
            .unwrap_or(1.0)
            * transform_stroke_scale(transform);
        let rgba = appearance
            .and_then(|appearance| appearance.get("strokeColor"))
            .and_then(Value::as_dictionary)
            .and_then(|color| color.get("rgba"))
            .and_then(parse_rgba_array)
            .unwrap_or([0, 0, 0, 255]);
        let style = appearance
            .and_then(|appearance| appearance.get("style"))
            .and_then(value_i64)
            .unwrap_or(3) as u8;
        let dash_pattern = appearance
            .and_then(|appearance| appearance.get("dashStyle"))
            .and_then(Value::as_dictionary)
            .and_then(|dash| dash.get("pattern"))
            .and_then(value_i64)
            .map(|pattern| pattern as u8);
        if kind == "partialshape" {
            let Some((path_commands, points)) = shape
                .get("strokePath")
                .and_then(Value::as_data)
                .and_then(parse_notability_path)
            else {
                continue;
            };
            let points: Vec<(f32, f32)> = points
                .into_iter()
                .map(|point| apply_transform(point, transform))
                .collect();
            curves.extend(super::geometry::stroke_pages(
                StrokeCurve {
                    page_index: 0,
                    points,
                    preserve_vertices: true,
                    path_commands,
                    fill_path: true,
                    width: stroke_width,
                    rgba,
                    style,
                    dash_pattern,
                    pressures: Vec::new(),
                    fractional_widths: Vec::new(),
                    opacities: Vec::new(),
                },
                page_height_doc,
            ));
            continue;
        }
        let Some((points, preserve_vertices)) = parse_shape_points(kind, shape) else {
            let Some((x, y, width, height)) = shape.get("rect").and_then(parse_nested_rect) else {
                continue;
            };
            let points = vec![(x, y + height / 2.0), (x + width, y + height / 2.0)]
                .into_iter()
                .map(|point| apply_transform(point, transform))
                .collect();
            curves.extend(split_curve_into_pages(
                points,
                false,
                stroke_width,
                rgba,
                style,
                dash_pattern,
                vec![1.0, 1.0],
                vec![1.0, 1.0],
                page_height_doc,
            ));
            continue;
        };
        let points: Vec<(f32, f32)> = points
            .into_iter()
            .map(|point| apply_transform(point, transform))
            .collect();
        let sample_count = bezier_sample_count(points.len());
        curves.extend(split_curve_into_pages(
            points,
            preserve_vertices,
            stroke_width,
            rgba,
            style,
            dash_pattern,
            vec![1.0; sample_count],
            vec![1.0; sample_count],
            page_height_doc,
        ));
    }
    curves
}

fn parse_shape_points(kind: &str, shape: &Dictionary) -> Option<(Vec<(f32, f32)>, bool)> {
    match kind {
        "square" | "rectangle" | "triangle" | "polygon" => {
            let mut points = shape
                .get("points")
                .and_then(parse_nested_points)
                .or_else(|| shape.get("rotatedRect").and_then(parse_rotated_rect_points))
                .or_else(|| shape.get("rect").and_then(parse_rect_points))?;
            close_shape_points(&mut points, shape.get("isClosed"));
            Some((points, true))
        }
        "circle" | "ellipse" => {
            let (center, radius_x, radius_y) = circle_geometry(shape)?;
            Some((ellipse_bezier_points(center, radius_x, radius_y), false))
        }
        "line" => parse_shape_line_points(shape),
        _ => None,
    }
}

fn parse_notability_path(data: &[u8]) -> Option<(Vec<u8>, Vec<(f32, f32)>)> {
    if data.len() < 8 {
        return None;
    }
    let command_count = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?) as usize;
    let commands = data.get(8..8 + command_count)?.to_vec();
    let point_count = commands
        .iter()
        .try_fold(0usize, |count, command| match command {
            0 | 1 => Some(count + 1),
            3 => Some(count + 3),
            _ => None,
        })?;
    let point_bytes = data.get(8 + command_count..)?;
    if point_bytes.len() != point_count * 16 {
        return None;
    }
    let mut points = Vec::with_capacity(point_count);
    for chunk in point_bytes.chunks_exact(16) {
        let x = f64::from_le_bytes(chunk.get(0..8)?.try_into().ok()?) as f32;
        let y = f64::from_le_bytes(chunk.get(8..16)?.try_into().ok()?) as f32;
        points.push((x, y));
    }
    Some((commands, points))
}

fn parse_shape_line_points(shape: &Dictionary) -> Option<(Vec<(f32, f32)>, bool)> {
    let start = shape.get("startPt").and_then(parse_value_pair)?;
    let end = shape.get("endPt").and_then(parse_value_pair)?;
    if let Some(control2) = shape.get("controlPoint2").and_then(parse_value_pair) {
        let control1 = shape
            .get("controlPoint1")
            .and_then(parse_value_pair)
            .unwrap_or(control2);
        return Some((vec![start, control1, control2, end], false));
    }
    if let Some(control) = shape.get("controlPoint1").and_then(parse_value_pair) {
        let c1 = (
            start.0 + (control.0 - start.0) * (2.0 / 3.0),
            start.1 + (control.1 - start.1) * (2.0 / 3.0),
        );
        let c2 = (
            end.0 + (control.0 - end.0) * (2.0 / 3.0),
            end.1 + (control.1 - end.1) * (2.0 / 3.0),
        );
        return Some((vec![start, c1, c2, end], false));
    }
    Some((vec![start, end], true))
}

fn parse_nested_points(value: &Value) -> Option<Vec<(f32, f32)>> {
    let points = value.as_array()?;
    let points: Vec<(f32, f32)> = points.iter().filter_map(parse_value_pair).collect();
    (!points.is_empty()).then_some(points)
}

fn parse_value_pair(value: &Value) -> Option<(f32, f32)> {
    let pair = value.as_array()?;
    Some((value_f32(pair.first()?)?, value_f32(pair.get(1)?)?))
}

fn parse_rotated_rect_points(value: &Value) -> Option<Vec<(f32, f32)>> {
    let corners = value
        .as_dictionary()?
        .get("corners")
        .and_then(parse_nested_points)?;
    Some(corners)
}

fn parse_rect_points(value: &Value) -> Option<Vec<(f32, f32)>> {
    let (x, y, width, height) = parse_nested_rect(value)?;
    Some(vec![
        (x, y),
        (x + width, y),
        (x + width, y + height),
        (x, y + height),
    ])
}

fn close_shape_points(points: &mut Vec<(f32, f32)>, is_closed: Option<&Value>) {
    if is_closed.and_then(Value::as_boolean).unwrap_or(false)
        && points
            .first()
            .zip(points.last())
            .is_some_and(|(first, last)| first != last)
    {
        points.push(points[0]);
    }
}

fn circle_geometry(shape: &Dictionary) -> Option<((f32, f32), f32, f32)> {
    if let Some(corners) = shape.get("rotatedRect").and_then(parse_rotated_rect_points) {
        let (sum_x, sum_y) = corners
            .iter()
            .fold((0.0f32, 0.0f32), |(sum_x, sum_y), point| {
                (sum_x + point.0, sum_y + point.1)
            });
        let center = (sum_x / corners.len() as f32, sum_y / corners.len() as f32);
        let radius_x = corners
            .first()
            .zip(corners.get(1))
            .map(|(a, b)| ((b.0 - a.0).hypot(b.1 - a.1)) * 0.5)?;
        let radius_y = corners
            .get(1)
            .zip(corners.get(2))
            .map(|(a, b)| ((b.0 - a.0).hypot(b.1 - a.1)) * 0.5)?;
        return Some((center, radius_x, radius_y));
    }
    let (x, y, width, height) = shape.get("rect").and_then(parse_nested_rect)?;
    Some((
        (x + width * 0.5, y + height * 0.5),
        width * 0.5,
        height * 0.5,
    ))
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

fn parse_nested_rect(value: &Value) -> Option<(f32, f32, f32, f32)> {
    let rect = value.as_array()?;
    let origin = rect.first()?.as_array()?;
    let size = rect.get(1)?.as_array()?;
    Some((
        value_f32(origin.first()?)?,
        value_f32(origin.get(1)?)?,
        value_f32(size.first()?)?,
        value_f32(size.get(1)?)?,
    ))
}

fn parse_rgba_array(value: &Value) -> Option<[u8; 4]> {
    let rgba = value.as_array()?;
    if rgba.len() < 4 {
        return None;
    }
    Some([
        unit_to_u8(value_f32(&rgba[0])?),
        unit_to_u8(value_f32(&rgba[1])?),
        unit_to_u8(value_f32(&rgba[2])?),
        unit_to_u8(value_f32(&rgba[3])?),
    ])
}

fn parse_transform(value: &Value) -> Option<[f32; 6]> {
    let array = value.as_array()?;
    if array.len() < 6 {
        return None;
    }
    Some([
        value_f32(&array[0])?,
        value_f32(&array[1])?,
        value_f32(&array[2])?,
        value_f32(&array[3])?,
        value_f32(&array[4])?,
        value_f32(&array[5])?,
    ])
}

fn apply_transform(point: (f32, f32), transform: Option<[f32; 6]>) -> (f32, f32) {
    let Some([a, b, c, d, tx, ty]) = transform else {
        return point;
    };
    (
        a * point.0 + c * point.1 + tx,
        b * point.0 + d * point.1 + ty,
    )
}

fn transform_stroke_scale(transform: Option<[f32; 6]>) -> f32 {
    let Some([a, b, c, d, _, _]) = transform else {
        return 1.0;
    };
    let x_scale = a.hypot(b);
    let y_scale = c.hypot(d);
    ((x_scale + y_scale) * 0.5).max(0.01)
}

fn default_if_empty(values: Vec<f32>, count: usize) -> Vec<f32> {
    if values.is_empty() {
        vec![1.0; count]
    } else {
        values
    }
}

fn split_curve_into_pages(
    points: Vec<(f32, f32)>,
    preserve_vertices: bool,
    width: f32,
    rgba: [u8; 4],
    style: u8,
    dash_pattern: Option<u8>,
    pressures: Vec<f32>,
    fractional_widths: Vec<f32>,
    page_height_doc: f32,
) -> Vec<StrokeCurve> {
    super::geometry::stroke_pages(
        StrokeCurve {
            page_index: 0,
            points,
            preserve_vertices,
            path_commands: Vec::new(),
            fill_path: false,
            width,
            rgba,
            style,
            dash_pattern,
            pressures,
            fractional_widths,
            opacities: Vec::new(),
        },
        page_height_doc,
    )
}

fn bezier_sample_count(point_count: usize) -> usize {
    if point_count > 0 && (point_count - 1) % 3 == 0 {
        ((point_count - 1) / 3) + 1
    } else {
        point_count
    }
}

fn parse_handwriting_color(raw: &[u8]) -> [u8; 4] {
    if raw.len() == 4 {
        [raw[0], raw[1], raw[2], raw[3]]
    } else {
        [0, 0, 0, 255]
    }
}

fn value_data<'a>(archive: Option<&'a KeyedArchive>, value: Option<&'a Value>) -> Option<&'a [u8]> {
    let value = value?;
    let value = archive.map(|archive| archive.deref(value)).unwrap_or(value);
    value.as_data()
}

fn unpack_f32(archive: Option<&KeyedArchive>, value: Option<&Value>) -> Vec<f32> {
    value_data(archive, value)
        .unwrap_or(&[])
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
        .collect()
}

fn unpack_i32(archive: Option<&KeyedArchive>, value: Option<&Value>) -> Vec<i32> {
    value_data(archive, value)
        .unwrap_or(&[])
        .chunks_exact(4)
        .map(|chunk| i32::from_le_bytes(chunk.try_into().unwrap()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::locked_export_width;

    fn dictionary(entries: Vec<(&str, plist::Value)>) -> plist::Value {
        plist::Value::Dictionary(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        )
    }

    #[test]
    fn erased_fragments_keep_their_original_stroke_style_and_dash() {
        use plist::{Dictionary, Value};
        let mut patterns = Vec::new();
        dictionary(vec![(
            "objectPatterns",
            dictionary(vec![
                ("1", dictionary(vec![("pattern", Value::Integer(2.into()))])),
                ("2", dictionary(vec![("pattern", Value::Integer(3.into()))])),
            ]),
        )])
        .to_writer_binary(&mut patterns)
        .unwrap();
        let hash: Dictionary = [
            (
                "curvesnumpoints",
                Value::Data([2i32; 4].into_iter().flat_map(i32::to_le_bytes).collect()),
            ),
            (
                "curvespoints",
                Value::Data(
                    [
                        10f32, 10., 20., 20., 30., 30., 40., 40., 50., 50., 60., 60., 70., 70.,
                        80., 80.,
                    ]
                    .into_iter()
                    .flat_map(f32::to_le_bytes)
                    .collect(),
                ),
            ),
            ("curvesstyles", Value::Data(vec![3, 4, 5])),
            (
                "curveUUIDs",
                Value::Data([1u8, 2, 2, 3].into_iter().flat_map(|id| [id; 16]).collect()),
            ),
            ("dashStyles", Value::Data(patterns)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let curves = super::parse_curves_from_spatial_hash(None, &hash, 100., None);
        assert_eq!(
            curves.iter().map(|c| c.style).collect::<Vec<_>>(),
            [3, 4, 4, 5]
        );
        assert_eq!(
            curves.iter().map(|c| c.dash_pattern).collect::<Vec<_>>(),
            [None, Some(2), Some(2), Some(3)]
        );
        let ids: Vec<u8> = [1u8, 2, 2, 3].into_iter().flat_map(|id| [id; 16]).collect();
        assert_eq!(super::stroke_numbers(&ids, 4, 4), [0, 1, 2, 3]);
        assert_eq!(super::stroke_numbers(&ids, 4, 1), [0, 1, 2, 3]);
        assert_eq!(super::stroke_numbers(&[], 4, 3), [0, 1, 2, 3]);
    }

    #[test]
    fn apple_text_ranges_count_surrogate_pairs_as_two_units() {
        use plist::{Dictionary, Value};
        let rich: Dictionary = [(
            "attributedString".to_owned(),
            dictionary(vec![
                ("stringKey", Value::String("A😀B".into())),
                (
                    "subRangesKey",
                    dictionary(vec![(
                        "NS.objects",
                        Value::Array(vec![dictionary(vec![
                            ("subRangeRangeKey", Value::String("{3, 1}".into())),
                            (
                                "subRangeFontKey",
                                dictionary(vec![(
                                    "NSFontNameAttribute",
                                    Value::String("Helvetica-Bold".into()),
                                )]),
                            ),
                        ])]),
                    )]),
                ),
            ]),
        )]
        .into_iter()
        .collect();
        let archive = super::KeyedArchive {
            objects: vec![],
            root: Value::Dictionary(rich.clone()),
        };
        let (text, spans) = super::parse_text(&archive, &rich);
        assert_eq!(text, "A😀B");
        assert_eq!((spans[0].start, spans[0].length), (2, 1));
        assert!(spans[0].style.bold);
    }

    #[test]
    fn plain_text_boxes_do_not_gain_a_yellow_sticky_background() {
        use plist::{Dictionary, Value};
        let media = |color| {
            let mut entries = vec![
                ("textStore", dictionary(vec![])),
                ("documentOrigin", Value::String("{10, 20}".into())),
                ("unscaledContentSize", Value::String("{100, 100}".into())),
            ];
            let paper = if color {
                vec![("paperColor", Value::String("1,0,0,1".into()))]
            } else {
                vec![]
            };
            entries.push(("paperStyleObject", dictionary(paper)));
            dictionary(entries)
        };
        let rich: Dictionary = [(
            "mediaObjects".to_owned(),
            dictionary(vec![(
                "NS.objects",
                Value::Array(vec![media(false), media(true)]),
            )]),
        )]
        .into_iter()
        .collect();
        let archive = super::KeyedArchive {
            objects: vec![],
            root: Value::Dictionary(rich.clone()),
        };
        let backgrounds = super::parse_sticky_notes(&archive, &rich, 900.);
        assert_eq!(backgrounds.len(), 1);
        assert_eq!(backgrounds[0].color, [255, 0, 0, 255]);
    }

    #[test]
    fn ellipse_shape_alias_keeps_curved_geometry() {
        use plist::Value;
        let shape = dictionary(vec![(
            "rect",
            Value::Array(vec![
                Value::Array(vec![Value::Real(10.), Value::Real(20.)]),
                Value::Array(vec![Value::Real(80.), Value::Real(40.)]),
            ]),
        )]);
        let shape = shape.as_dictionary().unwrap();
        let circle = super::parse_shape_points("circle", shape).unwrap();
        let ellipse = super::parse_shape_points("ellipse", shape).unwrap();
        assert_eq!(circle, ellipse);
        assert!(!ellipse.1);
    }

    #[test]
    fn reads_locked_export_width() {
        assert_eq!(
            locked_export_width(Some("lockedWidth:679.0:iPad")),
            Some(679.0)
        );
    }

    #[test]
    fn rejects_invalid_locked_export_width() {
        assert_eq!(locked_export_width(Some("staticWidth")), None);
        assert_eq!(locked_export_width(Some("lockedWidth:0:iPad")), None);
        assert_eq!(
            locked_export_width(Some("lockedWidth:not-a-number:iPad")),
            None
        );
    }

    #[test]
    fn legacy_strokes_keep_their_continuation_across_page_boundaries() {
        for (points, preserve) in [
            (vec![(10.0, 90.0), (10.0, 110.0)], true),
            (
                vec![(10.0, 90.0), (10.0, 96.0), (10.0, 104.0), (10.0, 110.0)],
                false,
            ),
        ] {
            let curves = super::split_curve_into_pages(
                points.clone(),
                preserve,
                1.0,
                [0, 0, 0, 255],
                3,
                None,
                vec![1.0, 1.0],
                vec![1.0, 1.0],
                100.0,
            );
            assert_eq!(curves.len(), 2);
            assert_eq!(curves[0].points, points);
            assert_eq!(curves[1].page_index, 1);
            assert_eq!(curves[1].points.len(), points.len());
            assert_eq!(curves[1].points[0], (10.0, -10.0));
            assert_eq!(curves[1].points.last(), Some(&(10.0, 10.0)));
        }
    }

    #[test]
    fn exported_snapshots_do_not_receive_editor_flips_a_second_time() {
        use super::{Dictionary, KeyedArchive, Value};
        fn dict(entries: Vec<(&str, Value)>) -> Value {
            Value::Dictionary(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.to_owned(), value))
                    .collect(),
            )
        }
        let mut objects = vec![Value::String("$null".into())];
        for (horizontal, vertical) in [(true, false), (false, true), (true, true)] {
            objects.push(dict(vec![
                ("documentOrigin", Value::String("{10, 20}".into())),
                ("unscaledContentSize", Value::String("{100, 200}".into())),
                ("rotationDegrees", Value::Real(std::f64::consts::FRAC_PI_4)),
                ("isFlippedHorizontal", Value::Boolean(horizontal)),
                ("isFlippedVertical", Value::Boolean(vertical)),
                (
                    "figure",
                    dict(vec![(
                        "FigureBackgroundObjectKey",
                        dict(vec![(
                            "kImageObjectSnapshotKey",
                            dict(vec![(
                                "relativePath",
                                Value::String("Images/snapshot.jpg".into()),
                            )]),
                        )]),
                    )]),
                ),
            ]));
        }
        let rich_text: Dictionary = [(
            "mediaObjects".to_owned(),
            dict(vec![(
                "NS.objects",
                Value::Array(
                    (1..=3)
                        .map(|index| Value::Uid(plist::Uid::new(index)))
                        .collect(),
                ),
            )]),
        )]
        .into_iter()
        .collect();
        let archive = KeyedArchive {
            objects,
            root: Value::Dictionary(rich_text.clone()),
        };
        let images = super::parse_media_images(&archive, &rich_text, 900.0);
        assert_eq!(images.len(), 3);
        for image in images {
            assert!(!image.flipped_horizontal && !image.flipped_vertical);
            assert_eq!(
                (image.x, image.y, image.width, image.height),
                (10.0, 20.0, 100.0, 200.0)
            );
            assert!((image.rotation_degrees - 45.0).abs() < 0.0001);
        }
    }
}
