use super::archive::KeyedArchive;
use super::model::{
    EmbeddedPdfPage, ImageCrop, MediaImage, NoteDocument, StickyNote, StrokeCurve, TextBlock,
    TextSpan, TextStyle, DEFAULT_PAGE_RATIO,
};
use super::util::{choose_export_width, parse_pair, parse_rect, value_f32, value_i64};
use crate::pdf::{page_box, page_id_by_index};
use crate::Result;
use image::GenericImageView;
use lopdf::Document;
use plist::{Dictionary, Value};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;
use zip::ZipArchive;

pub(crate) fn load_note_document(note_path: &Path) -> Result<NoteDocument> {
    let file = File::open(note_path)?;
    let mut bundle = ZipArchive::new(file)?;
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
    let reflow_state_value =
        archive.deref(rich_text.get("reflowState").ok_or("richText has no reflowState")?);
    let reflow_state = reflow_state_value
        .as_dictionary()
        .ok_or("Unexpected reflowState object")?;
    let page_width_doc = reflow_state
        .get("pageWidthInDocumentCoordsKey")
        .and_then(value_f32)
        .unwrap_or(679.0);

    let (line_style, paper_size, sizing_behavior) = parse_paper_attributes(&archive, session);
    let export_width_pt = choose_export_width(paper_size.as_deref());
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

pub(crate) fn read_zip_entry(bundle: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>> {
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

fn thumbnail_ratio(bundle: &mut ZipArchive<File>, entry: &str) -> Result<f32> {
    let data = read_zip_entry(bundle, entry)?;
    let image = image::load_from_memory(&data)?;
    let (width, height) = image.dimensions();
    Ok(height as f32 / width as f32)
}

fn pdf_page_ratio(
    bundle: &mut ZipArchive<File>,
    bundle_root: &str,
    page: &EmbeddedPdfPage,
) -> Result<f32> {
    let data = read_zip_entry(bundle, &(bundle_root.to_owned() + &page.relative_path))?;
    let doc = Document::load_mem(&data)?;
    let page_id = page_id_by_index(&doc, page.source_page_index)?;
    let bbox = page_box(&doc, page_id)?;
    Ok((bbox[3] - bbox[1]) / (bbox[2] - bbox[0]))
}

fn parse_text(archive: &KeyedArchive, rich_text: &Dictionary) -> (String, Vec<TextSpan>) {
    let attributed = archive.ns_dict(rich_text.get("attributedString"));
    let text = attributed
        .get("stringKey")
        .and_then(|value| archive.as_text(value).ok())
        .unwrap_or_default();
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
            start: start as usize,
            length: length as usize,
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
            flipped_horizontal: media
                .get("isFlippedHorizontal")
                .and_then(Value::as_boolean)
                .unwrap_or(false),
            flipped_vertical: media
                .get("isFlippedVertical")
                .and_then(Value::as_boolean)
                .unwrap_or(false),
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
        let width = widths.get(curve_index).copied().unwrap_or(1.0)
            * transform_stroke_scale(transform);
        let style = styles.get(curve_index).copied().unwrap_or(3);
        let dash_pattern = dash_patterns.get(&curve_index).copied();
        let rgba =
            parse_handwriting_color(colors.get(curve_index * 4..curve_index * 4 + 4).unwrap_or(&[]));
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
            let page_index = points
                .first()
                .map(|point| (point.1 / page_height_doc).floor() as usize)
                .unwrap_or(0);
            let points: Vec<(f32, f32)> = points
                .into_iter()
                .map(|point| localize(point, page_index, page_height_doc))
                .collect();
            curves.push(StrokeCurve {
                page_index,
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
            });
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
        "circle" => {
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
    let point_count = commands.iter().try_fold(0usize, |count, command| {
        match command {
            0 | 1 => Some(count + 1),
            3 => Some(count + 3),
            _ => None,
        }
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
    if is_closed
        .and_then(Value::as_boolean)
        .unwrap_or(false)
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
    Some(((x + width * 0.5, y + height * 0.5), width * 0.5, height * 0.5))
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
    (a * point.0 + c * point.1 + tx, b * point.0 + d * point.1 + ty)
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
    if points.is_empty() {
        return Vec::new();
    }
    let expected = bezier_sample_count(points.len());
    if is_bezier_point_count(points.len())
        && pressures.len() == expected
        && fractional_widths.len() == expected
    {
        return split_bezier_curve_into_pages(
            points,
            preserve_vertices,
            width,
            rgba,
            style,
            dash_pattern,
            pressures,
            fractional_widths,
            page_height_doc,
        );
    }
    let mut curves = Vec::new();
    let mut current_page = (points[0].1 / page_height_doc).floor() as usize;
    let mut current_points = Vec::new();
    let mut current_pressures = Vec::new();
    let mut current_fractional = Vec::new();
    for (index, (x, y)) in points.iter().copied().enumerate() {
        let page_index = (y / page_height_doc).floor() as usize;
        let local_point = (x, y - page_index as f32 * page_height_doc);
        if page_index != current_page && !current_points.is_empty() {
            curves.push(StrokeCurve {
                page_index: current_page,
                points: current_points,
                preserve_vertices,
                path_commands: Vec::new(),
                fill_path: false,
                width,
                rgba,
                style,
                dash_pattern,
                pressures: current_pressures,
                fractional_widths: current_fractional,
            });
            current_points = vec![local_point];
            current_pressures = vec![pressures.get(index).copied().unwrap_or(1.0)];
            current_fractional = vec![fractional_widths.get(index).copied().unwrap_or(1.0)];
            current_page = page_index;
        } else {
            current_points.push(local_point);
            current_pressures.push(pressures.get(index).copied().unwrap_or(1.0));
            current_fractional.push(fractional_widths.get(index).copied().unwrap_or(1.0));
            current_page = page_index;
        }
    }
    if !current_points.is_empty() {
        curves.push(StrokeCurve {
            page_index: current_page,
            points: current_points,
            preserve_vertices,
            path_commands: Vec::new(),
            fill_path: false,
            width,
            rgba,
            style,
            dash_pattern,
            pressures: current_pressures,
            fractional_widths: current_fractional,
        });
    }
    curves
}

fn split_bezier_curve_into_pages(
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
    let mut curves = Vec::new();
    let mut current_page = None;
    let mut current_points: Vec<(f32, f32)> = Vec::new();
    let mut current_pressures = Vec::new();
    let mut current_fractional = Vec::new();
    for segment_index in 0..((points.len() - 1) / 3) {
        let start = points[segment_index * 3];
        let c1 = points[segment_index * 3 + 1];
        let c2 = points[segment_index * 3 + 2];
        let end = points[segment_index * 3 + 3];
        let page_index = (start.1 / page_height_doc).floor() as usize;
        if current_page != Some(page_index) {
            if let Some(page) = current_page {
                curves.push(StrokeCurve {
                    page_index: page,
                    points: current_points,
                    preserve_vertices,
                    path_commands: Vec::new(),
                    fill_path: false,
                    width,
                    rgba,
                    style,
                    dash_pattern,
                    pressures: current_pressures,
                    fractional_widths: current_fractional,
                });
                current_points = Vec::new();
                current_pressures = Vec::new();
                current_fractional = Vec::new();
            }
            current_page = Some(page_index);
            current_points.push(localize(start, page_index, page_height_doc));
            current_pressures.push(pressures[segment_index]);
            current_fractional.push(fractional_widths[segment_index]);
        }
        current_points.extend([
            localize(c1, page_index, page_height_doc),
            localize(c2, page_index, page_height_doc),
            localize(end, page_index, page_height_doc),
        ]);
        current_pressures.push(pressures[segment_index + 1]);
        current_fractional.push(fractional_widths[segment_index + 1]);
    }
    if let Some(page) = current_page {
        curves.push(StrokeCurve {
            page_index: page,
            points: current_points,
            preserve_vertices,
            path_commands: Vec::new(),
            fill_path: false,
            width,
            rgba,
            style,
            dash_pattern,
            pressures: current_pressures,
            fractional_widths: current_fractional,
        });
    }
    curves
}

fn localize(point: (f32, f32), page_index: usize, page_height_doc: f32) -> (f32, f32) {
    (point.0, point.1 - page_index as f32 * page_height_doc)
}

fn bezier_sample_count(point_count: usize) -> usize {
    if point_count > 0 && (point_count - 1) % 3 == 0 {
        ((point_count - 1) / 3) + 1
    } else {
        point_count
    }
}

fn is_bezier_point_count(point_count: usize) -> bool {
    point_count >= 4 && (point_count - 1) % 3 == 0
}

fn parse_handwriting_color(raw: &[u8]) -> [u8; 4] {
    if raw.len() == 4 {
        [raw[0], raw[1], raw[2], raw[3]]
    } else {
        [0, 0, 0, 255]
    }
}

fn value_data<'a>(
    archive: Option<&'a KeyedArchive>,
    value: Option<&'a Value>,
) -> Option<&'a [u8]> {
    let value = value?;
    let value = archive
        .map(|archive| archive.deref(value))
        .unwrap_or(value);
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
