use flate2::{write::ZlibEncoder, Compression};
use image::GenericImageView;
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use plist::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::ZipArchive;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const DEFAULT_TEXT_TOP_DOC: f32 = 3.0;
const DEFAULT_EXPORT_WIDTH_PT: f32 = 612.0;
const DEFAULT_PAGE_RATIO: f32 = 21.0 / 16.0;
const DEFAULT_CONTENT_INSET_RATIO: f32 = 1.0 / 38.4;
const DEFAULT_INPUT_DIR: &str = "input";
const DEFAULT_OUTPUT_DIR: &str = "output";
const STROKE_STYLE_HIGHLIGHTER: u8 = 4;
const STROKE_STYLE_PENCIL: u8 = 5;
const STROKE_STYLE_NOT_EXPORTED: u8 = 6;
const CURVE_SMOOTHING_TENSION: f32 = 0.8;

#[derive(Clone, Debug)]
struct TextStyle {
    font_size: f32,
    color: [u8; 4],
}

#[derive(Clone, Debug)]
struct TextSpan {
    start: usize,
    length: usize,
    style: TextStyle,
}

#[derive(Clone, Debug)]
struct MediaImage {
    page_index: usize,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    relative_path: String,
    z_index: i64,
}

#[derive(Clone, Debug)]
struct StrokeCurve {
    page_index: usize,
    points: Vec<(f32, f32)>,
    width: f32,
    rgba: [u8; 4],
    style: u8,
    pressures: Vec<f32>,
    fractional_widths: Vec<f32>,
}

#[derive(Clone, Debug)]
struct EmbeddedPdfPage {
    page_index: usize,
    relative_path: String,
    source_page_index: usize,
}

#[derive(Clone, Debug)]
struct NoteDocument {
    bundle_root: String,
    page_width_doc: f32,
    page_height_doc: f32,
    export_width_pt: f32,
    export_height_pt: f32,
    line_style: Option<String>,
    text: String,
    text_spans: Vec<TextSpan>,
    media_images: Vec<MediaImage>,
    pdf_pages: Vec<EmbeddedPdfPage>,
    curves: Vec<StrokeCurve>,
}

struct KeyedArchive {
    objects: Vec<Value>,
    root: Value,
}

impl KeyedArchive {
    fn new(payload: Value) -> Result<Self> {
        let dict = payload
            .as_dictionary()
            .ok_or("Session.plist root is not a keyed archive dictionary")?;
        let objects = dict
            .get("$objects")
            .and_then(Value::as_array)
            .ok_or("Session.plist has no $objects")?
            .clone();
        let top = dict
            .get("$top")
            .and_then(Value::as_dictionary)
            .ok_or("Session.plist has no $top")?;
        let root_ref = top.values().next().ok_or("Session.plist $top is empty")?;
        let root = deref_from_objects(&objects, root_ref).clone();
        Ok(Self { objects, root })
    }

    fn deref<'a>(&'a self, value: &'a Value) -> &'a Value {
        deref_from_objects(&self.objects, value)
    }

    fn ns_array(&self, value: Option<&Value>) -> Vec<Value> {
        let Some(value) = value else {
            return Vec::new();
        };
        let Some(dict) = self.deref(value).as_dictionary() else {
            return Vec::new();
        };
        let Some(objects) = dict.get("NS.objects").and_then(Value::as_array) else {
            return Vec::new();
        };
        objects.iter().map(|value| self.deref(value).clone()).collect()
    }

    fn ns_dict(&self, value: Option<&Value>) -> BTreeMap<String, Value> {
        let Some(value) = value else {
            return BTreeMap::new();
        };
        let value = self.deref(value);
        let Some(dict) = value.as_dictionary() else {
            return BTreeMap::new();
        };
        let Some(keys) = dict.get("NS.keys").and_then(Value::as_array) else {
            return dict.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        };
        let Some(objects) = dict.get("NS.objects").and_then(Value::as_array) else {
            return BTreeMap::new();
        };
        keys.iter()
            .zip(objects.iter())
            .filter_map(|(key, value)| Some((self.as_text(key).ok()?, self.deref(value).clone())))
            .collect()
    }

    fn as_text(&self, value: &Value) -> Result<String> {
        let value = self.deref(value);
        match value {
            Value::String(text) => Ok(text.clone()),
            Value::Data(data) => Ok(String::from_utf8(data.clone())?),
            Value::Dictionary(dict) => {
                if let Some(Value::Data(data)) = dict.get("NS.bytes") {
                    Ok(String::from_utf8(data.clone())?)
                } else {
                    Err("unsupported text payload dictionary".into())
                }
            }
            _ => Err("unsupported text payload".into()),
        }
    }
}

fn deref_from_objects<'a>(objects: &'a [Value], value: &'a Value) -> &'a Value {
    if let Some(uid) = value.as_uid() {
        objects.get(uid.get() as usize).unwrap_or(value)
    } else {
        value
    }
}

pub fn run_cli(args: Vec<String>) -> Result<()> {
    let output = parse_output_arg(&args)?;
    let input = PathBuf::from(DEFAULT_INPUT_DIR);
    if !input.exists() {
        fs::create_dir_all(&input)?;
        println!("Created input; add .note files there, then run again.");
        return Ok(());
    }
    if !input.is_dir() {
        return Err(format!("Input path is not a folder: {}", input.display()).into());
    }
    let output_dir = output.unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_DIR));
    mirror_folder(&input, &output_dir)?;
    Ok(())
}

fn parse_output_arg(args: &[String]) -> Result<Option<PathBuf>> {
    match args {
        [] => Ok(None),
        [flag, path] if flag == "--output" || flag == "-o" => Ok(Some(PathBuf::from(path))),
        _ => Err("usage: cargo run -- [--output <path>]".into()),
    }
}

fn mirror_folder(input: &Path, output: &Path) -> Result<()> {
    prepare_output_dir(input, output)?;
    let files = source_files(input)?;
    validate_collisions(input, output, &files)?;

    for entry in WalkDir::new(input).into_iter().filter_map(std::result::Result::ok) {
        if entry.file_type().is_dir() {
            let relative = entry.path().strip_prefix(input)?;
            fs::create_dir_all(output.join(relative))?;
        }
    }

    let mut count = 0usize;
    for source in files {
        let target = output_path_for_source(input, output, &source)?;
        if source.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("note")) {
            convert_note_file(&source, &target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &target)?;
        }
        count += 1;
    }
    println!("Mirrored {count} file(s) to {}", output.display());
    Ok(())
}

fn prepare_output_dir(input: &Path, output: &Path) -> Result<()> {
    let input = input.canonicalize()?;
    if output.exists() {
        let output_canon = output.canonicalize()?;
        if input == output_canon {
            return Err("Output folder must be different from input folder".into());
        }
        if input.starts_with(&output_canon) {
            return Err("Output folder must not contain the input folder".into());
        }
        if !output.is_dir() {
            return Err(format!("Output path exists and is not a folder: {}", output.display()).into());
        }
        fs::remove_dir_all(output)?;
    }
    fs::create_dir_all(output)?;
    Ok(())
}

fn source_files(input: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(input) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    Ok(files)
}

fn validate_collisions(input: &Path, output: &Path, files: &[PathBuf]) -> Result<()> {
    let mut targets = BTreeMap::new();
    for source in files {
        let target = output_path_for_source(input, output, source)?;
        if let Some(previous) = targets.insert(target.clone(), source.clone()) {
            return Err(format!(
                "Output path collision: {} and {} both map to {}",
                previous.strip_prefix(input)?.display(),
                source.strip_prefix(input)?.display(),
                target.strip_prefix(output)?.display()
            )
            .into());
        }
    }
    Ok(())
}

fn output_path_for_source(input: &Path, output: &Path, source: &Path) -> Result<PathBuf> {
    let relative = source.strip_prefix(input)?;
    if source.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("note")) {
        Ok(output.join(relative).with_extension("pdf"))
    } else {
        Ok(output.join(relative))
    }
}

fn convert_note_file(note_path: &Path, output_path: &Path) -> Result<()> {
    let note = load_note_document(note_path)?;
    write_vector_pdf(&note, note_path, output_path)
}

fn load_note_document(note_path: &Path) -> Result<NoteDocument> {
    let file = File::open(note_path)?;
    let mut bundle = ZipArchive::new(file)?;
    let entries: Vec<String> = bundle.file_names().map(str::to_owned).collect();
    let bundle_root = entries
        .iter()
        .find_map(|entry| entry.strip_suffix("Session.plist").map(str::to_owned))
        .ok_or("Could not find Session.plist inside .note bundle")?;
    let session_bytes = read_zip_entry(&mut bundle, &(bundle_root.clone() + "Session.plist"))?;
    let archive = KeyedArchive::new(Value::from_reader(Cursor::new(session_bytes))?)?;
    let session = archive.root.as_dictionary().ok_or("Unexpected Session.plist root object")?;
    let rich_text_value = archive.deref(session.get("richText").ok_or("Session has no richText")?);
    let rich_text = rich_text_value.as_dictionary().ok_or("Unexpected richText object")?;
    let reflow_state_value = archive.deref(rich_text.get("reflowState").ok_or("richText has no reflowState")?);
    let reflow_state = reflow_state_value.as_dictionary().ok_or("Unexpected reflowState object")?;
    let page_width_doc = reflow_state
        .get("pageWidthInDocumentCoordsKey")
        .and_then(value_f32)
        .unwrap_or(679.0);

    let (line_style, paper_size, sizing_behavior) = parse_paper_attributes(&archive, session);
    let export_width_pt = choose_export_width(paper_size.as_deref());
    let pdf_pages = parse_pdf_pages(&archive, rich_text);
    let page_ratio = if sizing_behavior.as_deref() == Some("staticWidth")
        && paper_size.as_deref().is_some_and(|size| size.eq_ignore_ascii_case("letter"))
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

fn read_zip_entry(bundle: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>> {
    let mut file = bundle.by_name(name)?;
    let mut data = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut data)?;
    Ok(data)
}

fn parse_paper_attributes(archive: &KeyedArchive, session: &plist::Dictionary) -> (Option<String>, Option<String>, Option<String>) {
    let paper_model = session.get("NBNoteTakingSessionDocumentPaperLayoutModelKey").map(|value| archive.deref(value));
    let paper_model = paper_model.and_then(Value::as_dictionary);
    let attrs = paper_model
        .and_then(|model| model.get("documentPaperAttributes"))
        .map(|value| archive.deref(value))
        .and_then(Value::as_dictionary);
    let Some(attrs) = attrs else {
        return (None, None, None);
    };
    let line_style = attrs.get("lineStyle2").and_then(|value| archive.as_text(value).ok());
    let paper_size = attrs.get("paperSize").and_then(|value| archive.as_text(value).ok());
    let sizing = attrs
        .get("paperSizingBehavior")
        .and_then(|value| archive.as_text(value).ok());
    (line_style, paper_size, sizing)
}

fn choose_export_width(paper_size: Option<&str>) -> f32 {
    match paper_size.map(str::to_ascii_lowercase).as_deref() {
        Some("a4") => 595.2756,
        Some("letter" | "legal") => 612.0,
        _ => DEFAULT_EXPORT_WIDTH_PT,
    }
}

fn choose_thumbnail_entry(entries: &[String], bundle_root: &str) -> Option<String> {
    entries
        .iter()
        .filter(|entry| {
            entry.starts_with(bundle_root)
                && entry.to_ascii_lowercase().ends_with(".png")
                && Path::new(entry).file_name().and_then(|name| name.to_str()).is_some_and(|name| {
                    name.to_ascii_lowercase().contains("thumb")
                })
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

fn pdf_page_ratio(bundle: &mut ZipArchive<File>, bundle_root: &str, page: &EmbeddedPdfPage) -> Result<f32> {
    let data = read_zip_entry(bundle, &(bundle_root.to_owned() + &page.relative_path))?;
    let doc = Document::load_mem(&data)?;
    let page_id = page_id_by_index(&doc, page.source_page_index)?;
    let bbox = page_box(&doc, page_id)?;
    Ok((bbox[3] - bbox[1]) / (bbox[2] - bbox[0]))
}

fn parse_text(archive: &KeyedArchive, rich_text: &plist::Dictionary) -> (String, Vec<TextSpan>) {
    let attributed = archive.ns_dict(rich_text.get("attributedString"));
    let text = attributed
        .get("stringKey")
        .and_then(|value| archive.as_text(value).ok())
        .unwrap_or_default();
    let mut spans = Vec::new();
    for raw_range in archive.ns_array(attributed.get("subRangesKey")) {
        let subrange = archive.ns_dict(Some(&raw_range));
        let Some(range_text) = subrange.get("subRangeRangeKey").and_then(|value| archive.as_text(value).ok()) else {
            continue;
        };
        let (start, length) = parse_pair(&range_text).unwrap_or((0.0, text.len() as f32));
        let font_attrs = archive.ns_dict(subrange.get("subRangeFontKey"));
        let font_size = font_attrs.get("NSFontSizeAttribute").and_then(value_f32).unwrap_or(12.0);
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
        let parts: Vec<f32> = text.split(',').filter_map(|part| part.parse::<f32>().ok()).collect();
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

fn parse_media_images(archive: &KeyedArchive, rich_text: &plist::Dictionary, page_height_doc: f32) -> Vec<MediaImage> {
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

fn parse_pdf_pages(archive: &KeyedArchive, rich_text: &plist::Dictionary) -> Vec<EmbeddedPdfPage> {
    let mut pages = Vec::new();
    for raw_layout in archive.ns_array(rich_text.get("pageLayoutArray")) {
        let layout = archive.ns_dict(Some(&raw_layout));
        let Some(document_page) = layout.get("kPageLayoutDocumentPageNumberKey").and_then(value_i64) else {
            continue;
        };
        let Some(source_page) = layout.get("kPageLayoutPDFPageNumberKey").and_then(value_i64) else {
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

fn parse_curves(archive: &KeyedArchive, rich_text: &plist::Dictionary, page_height_doc: f32) -> Vec<StrokeCurve> {
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
    let styles = spatial_hash.get("curvesstyles").and_then(Value::as_data).unwrap_or(&[]);
    let colors = spatial_hash.get("curvescolors").and_then(Value::as_data).unwrap_or(&[]);
    let sample_counts: Vec<usize> = counts.iter().map(|count| bezier_sample_count(*count as usize)).collect();
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
        let rgba = parse_handwriting_color(colors.get(curve_index * 4..curve_index * 4 + 4).unwrap_or(&[]));
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
            (default_if_empty(pressures, sample_count), default_if_empty(fractional_widths, sample_count))
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
    if is_bezier_point_count(points.len()) && pressures.len() == expected && fractional_widths.len() == expected {
        return split_bezier_curve_into_pages(points, width, rgba, style, pressures, fractional_widths, page_height_doc);
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

fn parse_pair(value: &str) -> Option<(f32, f32)> {
    let mut numbers = Vec::new();
    let mut current = String::new();
    for ch in value.chars() {
        if ch.is_ascii_digit() || ch == '-' || ch == '.' {
            current.push(ch);
        } else if !current.is_empty() {
            if let Ok(number) = current.parse::<f32>() {
                numbers.push(number);
            }
            current.clear();
        }
    }
    if !current.is_empty() {
        if let Ok(number) = current.parse::<f32>() {
            numbers.push(number);
        }
    }
    (numbers.len() >= 2).then(|| (numbers[0], numbers[1]))
}

fn value_f32(value: &Value) -> Option<f32> {
    match value {
        Value::Real(value) => Some(*value as f32),
        Value::Integer(value) => value.as_signed().map(|v| v as f32).or_else(|| value.as_unsigned().map(|v| v as f32)),
        _ => None,
    }
}

fn value_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(value) => value.as_signed().or_else(|| value.as_unsigned().map(|v| v as i64)),
        Value::Real(value) => Some(*value as i64),
        _ => None,
    }
}

fn parse_line_spacing_doc(line_style: Option<&str>, page_width_doc: f32, export_width_pt: f32) -> f32 {
    let Some(line_style) = line_style else {
        return 20.0;
    };
    let spacing_inches = line_style
        .split(':')
        .next_back()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(20.0);
    spacing_inches * 72.0 / (export_width_pt / page_width_doc)
}

fn page_count_for(note: &NoteDocument) -> usize {
    let mut page_count = 1usize;
    if let Some(max_page) = note.pdf_pages.iter().map(|page| page.page_index).max() {
        page_count = page_count.max(max_page + 1);
    }
    if let Some(max_page) = note.media_images.iter().map(|media| media.page_index).max() {
        page_count = page_count.max(max_page + 1);
    }
    if let Some(max_page) = note.curves.iter().map(|curve| curve.page_index).max() {
        page_count = page_count.max(max_page + 1);
    }
    page_count
}

fn write_vector_pdf(note: &NoteDocument, note_path: &Path, output_path: &Path) -> Result<()> {
    write_lopdf_pdf(note, note_path, output_path)
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
    for relative_path in note.pdf_pages.iter().map(|page| page.relative_path.clone()).collect::<BTreeSet<_>>() {
        let data = read_zip_entry(&mut bundle, &(note.bundle_root.clone() + &relative_path))?;
        let mut doc = Document::load_mem(&data)?;
        doc.renumber_objects_with(max_id);
        max_id = doc.max_id + 1;
        for (object_id, object) in doc.objects.iter() {
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
        let page_id = write_page(&mut output, &mut bundle, note, &loaded_pdfs, font_id, pages_id, page_index)?;
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
    let line_spacing_doc = parse_line_spacing_doc(note.line_style.as_deref(), note.page_width_doc, note.export_width_pt);
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
                    vec![sx.into(), 0.into(), 0.into(), sy.into(), (-bbox[0] * sx).into(), (-bbox[1] * sy).into()],
                ),
                Operation::new("Do", vec!["Base".into()]),
                Operation::new("Q", vec![]),
            ]);
        }
    } else {
        draw_background(&mut operations, note, doc_to_pt, content_inset_doc, line_spacing_doc);
    }

    if page_index == 0 {
        draw_text(&mut operations, note, font_id, doc_to_pt, content_inset_doc, line_spacing_doc);
    }
    for (image_index, media) in note.media_images.iter().filter(|media| media.page_index == page_index).enumerate() {
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
    let curves: Vec<&StrokeCurve> = note.curves.iter().filter(|curve| curve.page_index == page_index).collect();
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
    let content_id = output.add_object(Stream::new(Dictionary::new(), Content { operations }.encode()?));
    let page_id = output.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), note.export_width_pt.into(), note.export_height_pt.into()],
    });
    Ok(page_id)
}

fn draw_background(operations: &mut Vec<Operation>, note: &NoteDocument, doc_to_pt: f32, content_inset_doc: f32, line_spacing_doc: f32) {
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
        Operation::new("RG", vec![(163.0 / 255.0).into(), (183.0 / 255.0).into(), (211.0 / 255.0).into()]),
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
        if content_inset_doc + cursor_x_doc + char_width_doc > note.page_width_doc - content_inset_doc {
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

fn add_image_xobject(output: &mut Document, bundle: &mut ZipArchive<File>, note: &NoteDocument, media: &MediaImage) -> Result<ObjectId> {
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
                draw_pencil_curve(operations, output, ext_gstates, ext_cache, curve, doc_to_pt, content_inset_doc, page_height);
            } else {
                draw_pen_curve(operations, output, ext_gstates, ext_cache, curve, doc_to_pt, content_inset_doc, page_height);
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
    let gs = ext_gstate_name(output, ext_gstates, ext_cache, curve.rgba[3] as f32 / 255.0, multiply);
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
            let alpha = (curve.rgba[3] as f32 / 255.0) * (0.18 + 0.34 * pressure).clamp(0.14, 0.96);
            let gs = ext_gstate_name(output, ext_gstates, ext_cache, alpha, false);
            push_stroke_state(operations, curve, width, &gs);
            let start = points[segment_index * 3];
            let c1 = points[segment_index * 3 + 1];
            let c2 = points[segment_index * 3 + 2];
            let end = points[segment_index * 3 + 3];
            operations.extend([
                Operation::new("m", vec![start.0.into(), start.1.into()]),
                Operation::new("c", vec![c1.0.into(), c1.1.into(), c2.0.into(), c2.1.into(), end.0.into(), end.1.into()]),
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
            let alpha = (curve.rgba[3] as f32 / 255.0) * (0.18 + 0.34 * pressure).clamp(0.14, 0.96);
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
                vec![c1.0.into(), c1.1.into(), c2.0.into(), c2.1.into(), end.0.into(), end.1.into()],
            ));
        }
    } else {
        if points.len() == 2 {
            operations.push(Operation::new("l", vec![points[1].0.into(), points[1].1.into()]));
        } else {
            for (c1, c2, end) in smoothed_cubic_segments(points) {
                operations.push(Operation::new(
                    "c",
                    vec![c1.0.into(), c1.1.into(), c2.0.into(), c2.1.into(), end.0.into(), end.1.into()],
                ));
            }
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

fn stroke_points_pt(curve: &StrokeCurve, doc_to_pt: f32, content_inset_doc: f32, page_height: f32) -> Vec<(f32, f32)> {
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

fn add_form_xobject(output: &mut Document, source_doc: &Document, page_id: ObjectId, bbox: [f32; 4]) -> Result<ObjectId> {
    let content = source_doc.get_page_content(page_id)?;
    let resources = inherited_page_object(source_doc, page_id, b"Resources").unwrap_or_else(|| Dictionary::new().into());
    let stream = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "FormType" => 1,
            "BBox" => bbox.iter().copied().map(Object::from).collect::<Vec<_>>(),
            "Resources" => resources,
        },
        content,
    );
    Ok(output.add_object(stream))
}

fn page_id_by_index(doc: &Document, zero_based_index: usize) -> Result<ObjectId> {
    doc.get_pages()
        .get(&((zero_based_index + 1) as u32))
        .copied()
        .ok_or_else(|| format!("PDF page index {zero_based_index} not found").into())
}

fn page_box(doc: &Document, page_id: ObjectId) -> Result<[f32; 4]> {
    let object = inherited_page_object(doc, page_id, b"MediaBox")
        .or_else(|| inherited_page_object(doc, page_id, b"CropBox"))
        .ok_or("page has no MediaBox")?;
    let values = object.as_array()?;
    if values.len() != 4 {
        return Err("page box must contain 4 numbers".into());
    }
    Ok([
        values[0].as_float()?,
        values[1].as_float()?,
        values[2].as_float()?,
        values[3].as_float()?,
    ])
}

fn inherited_page_object(doc: &Document, mut object_id: ObjectId, key: &[u8]) -> Option<Object> {
    for _ in 0..32 {
        let dictionary = doc.get_object(object_id).ok()?.as_dict().ok()?;
        if let Ok(object) = dictionary.get(key) {
            return Some(object.clone());
        }
        object_id = dictionary.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}
