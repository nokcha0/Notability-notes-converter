use super::archive::KeyedArchive;
use super::model::{
    EmbeddedPdfPage, MediaImage, NoteDocument, StrokeCurve, TextSpan, TextStyle,
    DEFAULT_PAGE_RATIO,
};
use super::util::{choose_export_width, parse_pair, value_f32, value_i64};
use crate::pdf::{page_box, page_id_by_index};
use crate::Result;
use image::GenericImageView;
use lopdf::Document;
use plist::{Dictionary, Value};
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
    let media_images = parse_media_images(&archive, rich_text, page_height_doc);
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
        media_images,
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
        let color = subrange
            .get("subRangeColorCrossPlatformKey")
            .or_else(|| subrange.get("subRangeColorKey"))
            .map(|value| parse_text_color(archive.deref(value)))
            .unwrap_or([0, 0, 0, 255]);
        spans.push(TextSpan {
            start: start as usize,
            length: length as usize,
            style: TextStyle { font_size, color },
        });
    }
    if spans.is_empty() {
        spans.push(TextSpan {
            start: 0,
            length: text.chars().count(),
            style: TextStyle {
                font_size: 12.0,
                color: [0, 0, 0, 255],
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
        let relative_path = media
            .get("figure")
            .map(|value| archive.deref(value))
            .and_then(Value::as_dictionary)
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
        let page_index = (y / page_height_doc).floor() as usize;
        images.push(MediaImage {
            page_index,
            x,
            y: y - page_index as f32 * page_height_doc,
            width,
            height,
            relative_path,
            z_index: media.get("zIndex").and_then(value_i64).unwrap_or(0),
        });
    }
    images.sort_by_key(|image| (image.page_index, image.z_index));
    images
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
    let counts = unpack_i32(spatial_hash.get("curvesnumpoints"));
    let widths = unpack_f32(spatial_hash.get("curveswidth"));
    let points_blob = unpack_f32(spatial_hash.get("curvespoints"));
    let pressures_blob = unpack_f32(spatial_hash.get("curvesforces"));
    let fractional_widths_blob = unpack_f32(spatial_hash.get("curvesfractionalwidths"));
    let styles = spatial_hash
        .get("curvesstyles")
        .and_then(Value::as_data)
        .unwrap_or(&[]);
    let colors = spatial_hash
        .get("curvescolors")
        .and_then(Value::as_data)
        .unwrap_or(&[]);
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
            points.push((points_blob[points_index], points_blob[points_index + 1]));
            points_index += 2;
        }
        let width = widths.get(curve_index).copied().unwrap_or(1.0);
        let style = styles.get(curve_index).copied().unwrap_or(3);
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
            width,
            rgba,
            style,
            pressures,
            fractional_widths,
            page_height_doc,
        ));
    }
    curves
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
    width: f32,
    rgba: [u8; 4],
    style: u8,
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
            width,
            rgba,
            style,
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
                width,
                rgba,
                style,
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
            width,
            rgba,
            style,
            pressures: current_pressures,
            fractional_widths: current_fractional,
        });
    }
    curves
}

fn split_bezier_curve_into_pages(
    points: Vec<(f32, f32)>,
    width: f32,
    rgba: [u8; 4],
    style: u8,
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
                    width,
                    rgba,
                    style,
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
            width,
            rgba,
            style,
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

fn unpack_f32(value: Option<&Value>) -> Vec<f32> {
    value
        .and_then(Value::as_data)
        .unwrap_or(&[])
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
        .collect()
}

fn unpack_i32(value: Option<&Value>) -> Vec<i32> {
    value
        .and_then(Value::as_data)
        .unwrap_or(&[])
        .chunks_exact(4)
        .map(|chunk| i32::from_le_bytes(chunk.try_into().unwrap()))
        .collect()
}
