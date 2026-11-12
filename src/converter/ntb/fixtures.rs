//! Small FlatBuffers/ZIP fixtures for capabilities absent from the example note.
use super::{load_ntb_document, FlatBuffer};
use crate::converter::{model::OutputFormat, render::write_note_output};
use lopdf::{
    content::{Content, Operation},
    dictionary, Dictionary, Document, Stream,
};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use zip::{write::SimpleFileOptions, ZipWriter};

struct Buffer(Vec<u8>);

impl Buffer {
    fn table(&mut self, slots: usize, fields: &[(usize, Vec<u8>)]) -> usize {
        while self.0.len() % 4 != 0 {
            self.0.push(0);
        }
        let vtable = self.0.len();
        self.0.extend(vec![0; 4 + slots * 2]);
        while self.0.len() % 4 != 0 {
            self.0.push(0);
        }
        let table = self.0.len();
        self.0.extend(((table - vtable) as i32).to_le_bytes());
        for (slot, bytes) in fields {
            while self.0.len() % 4 != 0 {
                self.0.push(0);
            }
            let offset = (self.0.len() - table) as u16;
            self.0[vtable + 4 + slot * 2..vtable + 6 + slot * 2]
                .copy_from_slice(&offset.to_le_bytes());
            self.0.extend(bytes);
        }
        let size = (self.0.len() - table) as u16;
        self.0[vtable..vtable + 2].copy_from_slice(&((4 + slots * 2) as u16).to_le_bytes());
        self.0[vtable + 2..vtable + 4].copy_from_slice(&size.to_le_bytes());
        table
    }

    fn link(&mut self, table: usize, slot: usize, target: usize) {
        let field = FlatBuffer::new(&self.0).field(table, slot).unwrap();
        assert!(target >= field);
        self.0[field..field + 4].copy_from_slice(&((target - field) as u32).to_le_bytes());
    }

    fn string(&mut self, text: &str) -> usize {
        while self.0.len() % 4 != 0 {
            self.0.push(0);
        }
        let offset = self.0.len();
        self.0.extend((text.len() as u32).to_le_bytes());
        self.0.extend(text.as_bytes());
        self.0.push(0);
        offset
    }
}

fn id(index: u32) -> Vec<u8> {
    [7u32, index]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect()
}

fn position(index: u32, offset: u32) -> Vec<u8> {
    [7u32, index, offset]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect()
}

fn pair(x: f32, y: f32) -> Vec<u8> {
    [x, y].into_iter().flat_map(f32::to_le_bytes).collect()
}

fn fixture() -> Vec<u8> {
    // Nonsequential ids, interleaved stores, and styles before their text.
    let kinds = [
        (0, 1),
        (10, 8),
        (60, 8),
        (300, 12),
        (100, 22),
        (200, 22),
        (50, 8),
        (301, 12),
        (11, 8),
        (99, 7),
    ];
    let mut b = Buffer(vec![0; 4]);
    let root = b.table(7, &[(6, vec![0; 4])]);
    let vector = b.0.len();
    b.0.extend((kinds.len() as u32).to_le_bytes());
    b.0.extend(vec![0; kinds.len() * 4]);
    b.link(root, 6, vector);
    b.0[..4].copy_from_slice(&(root as u32).to_le_bytes());
    let mut wrappers = Vec::new();
    for (index, kind) in kinds {
        wrappers.push(b.table(6, &[(0, id(index)), (4, vec![kind]), (5, vec![0; 4])]));
    }
    for (i, wrapper) in wrappers.iter().enumerate() {
        let slot = vector + 4 + i * 4;
        b.0[slot..slot + 4].copy_from_slice(&((*wrapper - slot) as u32).to_le_bytes());
    }
    let mut payloads = Vec::new();
    for (index, kind) in kinds {
        let fields = match kind {
            8 => vec![
                (
                    0,
                    id(match index {
                        50 => 100,
                        60 => 200,
                        _ => 0,
                    }),
                ),
                (1, vec![0; 4]),
            ],
            22 => vec![
                (2, position(1, if index == 100 { 3 } else { 1 })),
                (3, pair(if index == 100 { 40.0 } else { 220.0 }, 70.0)),
                (6, pair(200.0, 100.0)),
            ],
            12 => vec![
                (
                    0,
                    position(
                        if index == 300 { 50 } else { 60 },
                        if index == 300 { 3 } else { 0 },
                    ),
                ),
                (
                    1,
                    position(
                        if index == 300 { 50 } else { 60 },
                        if index == 300 { 8 } else { 1 },
                    ),
                ),
                (9, vec![0; 4]),
            ],
            7 => vec![(0, position(11, 4))],
            _ => vec![],
        };
        payloads.push(b.table(
            if kind == 22 {
                16
            } else if kind == 12 {
                14
            } else {
                3
            },
            &fields,
        ));
    }
    for (wrapper, payload) in wrappers.iter().zip(&payloads) {
        b.link(*wrapper, 5, *payload);
    }
    for ((index, kind), payload) in kinds.into_iter().zip(payloads) {
        if kind == 8 {
            let text = match index {
                10 => "Main ",
                11 => "body",
                50 => "A😀green",
                60 => "B red",
                _ => unreachable!(),
            };
            let string = b.string(text);
            b.link(payload, 1, string);
        } else if kind == 12 {
            let color = b.table(
                1,
                &[(
                    0,
                    if index == 300 {
                        vec![10, 200, 30, 255]
                    } else {
                        vec![200, 10, 30, 255]
                    },
                )],
            );
            b.link(payload, 9, color);
        }
    }
    b.0
}

fn image_fixture(crop: [f32; 4], logical_size: (f32, f32)) -> Vec<u8> {
    let mut b = Buffer(vec![0; 4]);
    let root = b.table(7, &[(6, vec![0; 4])]);
    let vector = b.0.len();
    b.0.extend(1u32.to_le_bytes());
    b.0.extend([0; 4]);
    b.link(root, 6, vector);
    b.0[..4].copy_from_slice(&(root as u32).to_le_bytes());
    let event = b.table(6, &[(0, id(42)), (4, vec![22]), (5, vec![0; 4])]);
    b.0[vector + 4..vector + 8].copy_from_slice(&((event - vector - 4) as u32).to_le_bytes());
    let payload = b.table(
        12,
        &[
            (0, vec![1]),
            (2, position(1, 0)),
            (3, pair(30.0, 40.0)),
            (6, pair(100.0, 50.0)),
            (10, vec![0; 4]),
            (11, crop.into_iter().flat_map(f32::to_le_bytes).collect()),
        ],
    );
    b.link(event, 5, payload);
    let media = b.table(
        2,
        &[(0, vec![0; 4]), (1, pair(logical_size.0, logical_size.1))],
    );
    b.link(payload, 10, media);
    let asset = b.table(3, &[(0, vec![0; 64]), (2, vec![0; 4])]);
    b.link(media, 0, asset);
    let mime = b.string("image/png");
    b.link(asset, 2, mime);
    b.0
}

fn image_asset() -> (String, Vec<u8>) {
    let image = image::RgbaImage::from_fn(20, 10, |x, y| {
        image::Rgba([x as u8 * 10, y as u8 * 20, 33, if x == 0 { 0 } else { 255 }])
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    (
        format!("assets/{}.png", "00".repeat(64)),
        bytes.into_inner(),
    )
}

struct Bundle {
    directory: PathBuf,
    path: PathBuf,
}

impl Bundle {
    fn new(bytes: &[u8], assets: &[(&str, Vec<u8>)]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "notability-fixture-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("fixture.ntb");
        let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
        zip.start_file("noteBundle", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
        for (name, data) in assets {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        Self { directory, path }
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn background_pdf(pages: usize) -> Vec<u8> {
    let mut pdf = Document::with_version("1.5");
    let tree = pdf.new_object_id();
    let mut kids = Vec::new();
    for _ in 0..pages {
        let content = Content {
            operations: vec![
                Operation::new("rg", vec![0.2.into(), 0.4.into(), 0.6.into()]),
                Operation::new("re", vec![0.into(), 0.into(), 100.into(), 200.into()]),
                Operation::new("f", vec![]),
            ],
        };
        let content = pdf.add_object(Stream::new(Dictionary::new(), content.encode().unwrap()));
        kids.push(
            pdf.add_object(
                dictionary! { "Type" => "Page", "Parent" => tree, "Contents" => content },
            ),
        );
    }
    pdf.objects.insert(tree, dictionary! { "Type" => "Pages", "Count" => pages as i64,
        "Kids" => kids.into_iter().map(Into::into).collect::<Vec<lopdf::Object>>(),
        "MediaBox" => vec![0.into(), 0.into(), 100.into(), 200.into()], "Resources" => Dictionary::new(),
    }.into());
    let root = pdf.add_object(dictionary! { "Type" => "Catalog", "Pages" => tree });
    pdf.trailer.set("Root", root);
    let mut bytes = Vec::new();
    pdf.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn links_interleaved_text_boxes_and_utf16_styles_by_event_id() {
    let bundle = Bundle::new(&fixture(), &[]);
    let note = load_ntb_document(&bundle.path).unwrap();
    assert_eq!(note.text, "Main body");
    assert_eq!(note.text_blocks.len(), 2);
    let a = note.text_blocks.iter().find(|b| b.page_index == 3).unwrap();
    let b = note.text_blocks.iter().find(|b| b.page_index == 1).unwrap();
    assert_eq!(a.text, "A😀green");
    assert_eq!(b.text, "B red");
    let green = a
        .text_spans
        .iter()
        .find(|s| s.style.color == [10, 200, 30, 255])
        .unwrap();
    assert_eq!((green.start, green.length), (2, 5));
    assert_eq!(b.text_spans[0].style.color, [200, 10, 30, 255]);
    assert_eq!(note.text_spans[0].style.color, [0, 0, 0, 255]);
    assert!((a.x + note.page_width_doc / 38.4 - 40.0).abs() < 0.001);
}

#[test]
fn logical_fractional_image_crops_preserve_source_pixels_and_display_size() {
    let (name, data) = image_asset();
    let bundle = Bundle::new(
        &image_fixture([1.25, 0.5, 4.25, 2.5], (10.0, 5.0)),
        &[(name.as_str(), data)],
    );
    let note = load_ntb_document(&bundle.path).unwrap();
    let image = &note.media_images[0];
    assert_eq!((image.width, image.height), (100.0, 50.0));
    assert_eq!(image.crop.unwrap().reference_size, Some((10.0, 5.0)));
    let output = bundle.directory.join("out.pdf");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Pdf).unwrap();
    let pdf = Document::load(output).unwrap();
    let page = *pdf.get_pages().get(&1).unwrap();
    let resources = pdf
        .get_object(page)
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Resources")
        .unwrap()
        .as_reference()
        .unwrap();
    let form_id = pdf
        .get_object(resources)
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"XObject")
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Im0")
        .unwrap()
        .as_reference()
        .unwrap();
    let form = pdf.get_object(form_id).unwrap().as_stream().unwrap();
    assert_eq!(
        form.dict.get(b"Subtype").unwrap().as_name().unwrap(),
        b"Form"
    );
    let ops = Content::decode(&form.content).unwrap().operations;
    let transform: Vec<_> = ops
        .iter()
        .find(|op| op.operator == "cm")
        .unwrap()
        .operands
        .iter()
        .map(|v| v.as_float().unwrap())
        .collect();
    let expected = [20.0 / 8.5, 0.0, 0.0, 2.0, -2.5 / 8.5, -0.8];
    for (actual, expected) in transform.iter().zip(expected) {
        assert!((actual - expected).abs() < 0.000001);
    }
    let source_id = form
        .dict
        .get(b"Resources")
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"XObject")
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Image")
        .unwrap()
        .as_reference()
        .unwrap();
    let source = pdf.get_object(source_id).unwrap().as_stream().unwrap();
    assert_eq!(source.dict.get(b"Width").unwrap().as_i64().unwrap(), 20);
    assert_eq!(source.dict.get(b"Height").unwrap().as_i64().unwrap(), 10);
    assert!(source.dict.get(b"Interpolate").unwrap().as_bool().unwrap());
    let mask_id = source.dict.get(b"SMask").unwrap().as_reference().unwrap();
    let mask = pdf.get_object(mask_id).unwrap().as_stream().unwrap();
    assert!(mask.dict.get(b"Interpolate").unwrap().as_bool().unwrap());
    let output = bundle.directory.join("out.svg");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Svg).unwrap();
    let svg = fs::read_to_string(output).unwrap();
    assert!(svg.contains("width=\"100.00000\" height=\"50.00000\""));
    assert!(svg.contains("viewBox=\"0 0 1 1\" preserveAspectRatio=\"none\" overflow=\"hidden\""));
    let inner = svg.split("<image ").nth(1).unwrap();
    for (name, expected) in [
        ("x", -2.5 / 8.5),
        ("y", -0.2),
        ("width", 20.0 / 8.5),
        ("height", 2.0),
    ] {
        let value: f32 = inner
            .split(&format!("{name}=\""))
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!((value - expected).abs() < 0.000001);
    }
}

#[test]
fn small_fractional_crops_are_not_mistaken_for_the_whole_asset() {
    let (name, data) = image_asset();
    let bundle = Bundle::new(
        &image_fixture([0.25, 0.0, 9.75, 5.0], (10.0, 5.0)),
        &[(name.as_str(), data)],
    );
    let note = load_ntb_document(&bundle.path).unwrap();
    let crop = note.media_images[0].crop.unwrap().in_pixels(20, 10);
    assert_eq!(
        (crop.x, crop.y, crop.width, crop.height),
        (0.5, 0.0, 19.5, 10.0)
    );
}

#[test]
fn renders_annotations_on_their_own_pages_and_preserves_blank_pdf_pages() {
    let bundle = Bundle::new(&fixture(), &[("assets/background.pdf", background_pdf(5))]);
    let note = load_ntb_document(&bundle.path).unwrap();
    assert_eq!(note.pdf_pages.len(), 5);
    let output = bundle.directory.join("out.pdf");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Pdf).unwrap();
    let pdf = Document::load(&output).unwrap();
    assert_eq!(pdf.get_pages().len(), 5);
    for (index, page) in pdf.get_pages().values().enumerate() {
        let content = Content::decode(&pdf.get_page_content(*page).unwrap()).unwrap();
        assert!(content
            .operations
            .iter()
            .any(|op| op.operator == "Do" && op.operands[0].as_name().unwrap() == b"Base"));
        let text: Vec<u8> = content
            .operations
            .iter()
            .filter(|op| op.operator == "Tj")
            .flat_map(|op| op.operands[0].as_str().unwrap().to_vec())
            .collect();
        match index {
            0 => assert_eq!(text, b"Main body"),
            1 => assert_eq!(text, b"B red"),
            3 => assert_eq!(String::from_utf8(text).unwrap(), "A😀green"),
            _ => assert!(text.is_empty()),
        }
    }
}

#[test]
fn browser_svg_loads_each_pdf_asset_once_and_requires_all_backgrounds() {
    let pdf = background_pdf(5);
    let bundle = Bundle::new(&fixture(), &[("assets/background.pdf", pdf.clone())]);
    let converter = crate::Converter::new(fs::read(&bundle.path).unwrap(), "fixture.ntb").unwrap();
    let backgrounds = converter.pdf_backgrounds().unwrap();
    assert_eq!(backgrounds.assets, [pdf]);
    assert_eq!(backgrounds.pages.len(), 5);
    for (index, page) in backgrounds.pages.iter().enumerate() {
        assert_eq!(page.asset_index, 0);
        assert_eq!(page.page_index, index);
    }
    assert!(converter.render_svg_with_backgrounds(&[]).is_err());
    assert!(converter
        .render_svg_with_backgrounds(&vec![b"not PNG".to_vec(); 5])
        .is_err());
    let (_, png) = image_asset();
    let svg = converter
        .render_svg_with_backgrounds(&vec![png; 5])
        .unwrap();
    assert_eq!(
        String::from_utf8(svg)
            .unwrap()
            .matches("data:image/png;base64,")
            .count(),
        5
    );
}

#[test]
fn renders_text_box_svg_with_page_local_coordinates() {
    let bundle = Bundle::new(&fixture(), &[]);
    let note = load_ntb_document(&bundle.path).unwrap();
    let output = bundle.directory.join("out.svg");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Svg).unwrap();
    let svg = fs::read_to_string(output).unwrap();
    let page = svg.split("id=\"page-4\"").nth(1).unwrap();
    assert!(page.contains(">A</text>"));
    assert!(page.contains("x=\"40.00\""));
    assert!(page.contains("text-clip-3-0"));
}

#[test]
fn preserves_main_text_pages_when_there_is_no_ink_or_media() {
    let bundle = Bundle::new(&fixture(), &[]);
    let mut note = load_ntb_document(&bundle.path).unwrap();
    note.text_blocks.clear();
    note.text = "Line\n".repeat(100);
    let output = bundle.directory.join("out.pdf");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Pdf).unwrap();
    let pdf = Document::load(output).unwrap();
    assert_eq!(pdf.get_pages().len(), 3);
    let text: Vec<u8> = pdf
        .get_pages()
        .values()
        .flat_map(|page| {
            Content::decode(&pdf.get_page_content(*page).unwrap())
                .unwrap()
                .operations
                .into_iter()
                .filter(|op| op.operator == "Tj")
                .flat_map(|op| op.operands[0].as_str().unwrap().to_vec())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(text, "Line".repeat(100).as_bytes());
}

#[test]
fn crossing_stroke_is_clipped_consistently_in_pdf_and_svg() {
    use crate::converter::{geometry::stroke_pages, model::StrokeCurve};
    let bundle = Bundle::new(&fixture(), &[]);
    let mut note = load_ntb_document(&bundle.path).unwrap();
    note.text.clear();
    note.text_blocks.clear();
    note.line_style = None;
    note.page_width_doc = 100.0;
    note.export_width_pt = 100.0;
    note.page_height_doc = 100.0;
    note.export_height_pt = 100.0;
    let inset = note.page_width_doc / 38.4;
    note.curves = stroke_pages(
        StrokeCurve {
            page_index: 0,
            points: vec![(50.0 - inset, 90.0), (50.0 - inset, 110.0)],
            preserve_vertices: true,
            path_commands: Vec::new(),
            fill_path: false,
            width: 1.0,
            rgba: [0, 0, 255, 255],
            style: 3,
            dash_pattern: None,
            pressures: vec![1.0, 1.0],
            fractional_widths: vec![1.0, 1.0],
            opacities: Vec::new(),
        },
        100.0,
    );
    let output = bundle.directory.join("out.pdf");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Pdf).unwrap();
    let pdf = Document::load(output).unwrap();
    assert_eq!(pdf.get_pages().len(), 2);
    for (index, page) in pdf.get_pages().values().enumerate() {
        let ops = Content::decode(&pdf.get_page_content(*page).unwrap())
            .unwrap()
            .operations;
        let start = ops.iter().find(|op| op.operator == "m").unwrap();
        let end = ops.iter().find(|op| op.operator == "l").unwrap();
        assert_eq!(
            start.operands[1].as_float().unwrap(),
            if index == 0 { 10.0 } else { 110.0 }
        );
        assert_eq!(
            end.operands[1].as_float().unwrap(),
            if index == 0 { -10.0 } else { 90.0 }
        );
    }
    let output = bundle.directory.join("out.svg");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Svg).unwrap();
    let svg = fs::read_to_string(output).unwrap();
    assert!(svg.contains("clip-path=\"url(#page-clip-1)\""));
    assert!(svg.contains("clip-path=\"url(#page-clip-2)\""));
    assert!(svg.contains("M 50.00 90.00 L 50.00 110.00"));
    assert!(svg.contains("M 50.00 -10.00 L 50.00 10.00"));
}

#[test]
fn refuses_to_guess_the_order_of_multiple_pdf_assets() {
    let bundle = Bundle::new(
        &fixture(),
        &[
            ("assets/a.pdf", background_pdf(1)),
            ("assets/b.pdf", background_pdf(1)),
        ],
    );
    let error = load_ntb_document(&bundle.path).unwrap_err().to_string();
    assert!(error.contains("multiple PDF assets"));
}

#[test]
fn draws_annotation_text_above_a_sticky_background_with_the_same_z_index() {
    use crate::converter::model::StickyNote;
    let bundle = Bundle::new(&fixture(), &[]);
    let mut note = load_ntb_document(&bundle.path).unwrap();
    let block = note.text_blocks.iter().find(|b| b.page_index == 1).unwrap();
    note.sticky_notes.push(StickyNote {
        page_index: block.page_index,
        x: block.x,
        y: block.y,
        width: block.width,
        height: block.height,
        rotation_degrees: 0.0,
        color: [255, 244, 142, 255],
        line_style: None,
        line_start_multiple: 2.0,
        z_index: block.z_index,
    });
    let output = bundle.directory.join("out.pdf");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Pdf).unwrap();
    let pdf = Document::load(output).unwrap();
    let page = *pdf.get_pages().get(&2).unwrap();
    let operations = Content::decode(&pdf.get_page_content(page).unwrap())
        .unwrap()
        .operations;
    let background = operations
        .iter()
        .rposition(|op| op.operator == "f")
        .unwrap();
    let text = operations
        .iter()
        .position(|op| op.operator == "Tj")
        .unwrap();
    assert!(background < text);
    let output = bundle.directory.join("out.svg");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Svg).unwrap();
    let svg = fs::read_to_string(output).unwrap();
    let page = svg.split("id=\"page-2\"").nth(1).unwrap();
    assert!(page.find("fill=\"rgb(255,244,142)\"").unwrap() < page.find(">B</text>").unwrap());
}

#[test]
#[ignore = "requires Poppler's pdftoppm"]
fn embeds_pdf_backgrounds_in_svg_without_external_asset_links() {
    let bundle = Bundle::new(&fixture(), &[("assets/background.pdf", background_pdf(5))]);
    let note = load_ntb_document(&bundle.path).unwrap();
    let output = bundle.directory.join("out.svg");
    write_note_output(&note, &bundle.path, &output, OutputFormat::Svg).unwrap();
    let svg = fs::read_to_string(output).unwrap();
    assert_eq!(svg.matches("data:image/png;base64,iVBOR").count(), 5);
    assert!(svg.contains("text-clip-3-0"));
}
