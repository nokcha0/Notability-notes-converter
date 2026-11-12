mod archive;
#[cfg(not(target_arch = "wasm32"))]
mod cli;
mod geometry;
mod model;
mod note;
mod ntb;
mod render;
mod util;

#[cfg(not(target_arch = "wasm32"))]
pub fn run_cli(args: Vec<String>) -> crate::Result<()> {
    cli::run(args)
}

use crate::Result;
use std::collections::BTreeMap;
use std::io::Cursor;
use zip::ZipArchive;

pub use model::OutputFormat;

/// A source PDF page needed when rendering SVG backgrounds outside Rust.
pub struct PdfBackground {
    pub asset_index: usize,
    pub page_index: usize,
}

#[derive(Default)]
pub struct PdfBackgrounds {
    pub assets: Vec<Vec<u8>>,
    pub pages: Vec<PdfBackground>,
}

/// A parsed note and its original media. No filesystem access is needed.
pub struct Converter {
    document: model::NoteDocument,
    source: Vec<u8>,
}

impl Converter {
    pub fn new(source: Vec<u8>, file_name: &str) -> Result<Self> {
        let extension = file_name
            .rsplit_once('.')
            .map_or("", |(_, extension)| extension);
        if !extension.eq_ignore_ascii_case("ntb") && !extension.eq_ignore_ascii_case("note") {
            return Err("Choose a .note or .ntb file".into());
        }
        let bundle = ZipArchive::new(Cursor::new(&source))?;
        let document = if extension.eq_ignore_ascii_case("ntb") {
            ntb::read_ntb_document(bundle)?
        } else {
            note::read_note_document(bundle)?
        };
        Ok(Self { document, source })
    }

    pub fn render(&self, format: OutputFormat) -> Result<Vec<u8>> {
        let mut bundle = ZipArchive::new(Cursor::new(&self.source))?;
        render::render_note(&self.document, &mut bundle, format)
    }

    pub fn pdf_backgrounds(&self) -> Result<PdfBackgrounds> {
        let mut bundle = ZipArchive::new(Cursor::new(&self.source))?;
        let mut indices = BTreeMap::new();
        let mut backgrounds = PdfBackgrounds::default();
        for page in &self.document.pdf_pages {
            let asset_index = if let Some(index) = indices.get(&page.relative_path) {
                *index
            } else {
                let index = backgrounds.assets.len();
                backgrounds.assets.push(note::read_zip_entry(
                    &mut bundle,
                    &(self.document.bundle_root.clone() + &page.relative_path),
                )?);
                indices.insert(page.relative_path.clone(), index);
                index
            };
            backgrounds.pages.push(PdfBackground {
                asset_index,
                page_index: page.source_page_index,
            });
        }
        Ok(backgrounds)
    }

    /// Backgrounds must be PNGs in the order returned by `pdf_backgrounds`.
    pub fn render_svg_with_backgrounds(&self, backgrounds: &[Vec<u8>]) -> Result<Vec<u8>> {
        if backgrounds.len() != self.document.pdf_pages.len() {
            return Err("Missing SVG PDF backgrounds".into());
        }
        let mut bundle = ZipArchive::new(Cursor::new(&self.source))?;
        render::render_svg_with_backgrounds(&self.document, &mut bundle, backgrounds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, path::Path};

    #[test]
    fn memory_and_file_readers_produce_identical_pdf_and_svg() {
        for name in ["NOTE Note Apr 26, 2026.note", "NTB Note Apr 26, 2026.ntb"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("input")
                .join(name);
            let bytes = std::fs::read(&path).unwrap();
            let memory = Converter::new(bytes, name).unwrap();
            let document = if name.ends_with("ntb") {
                ntb::load_ntb_document(&path).unwrap()
            } else {
                note::load_note_document(&path).unwrap()
            };
            for format in [OutputFormat::Pdf, OutputFormat::Svg] {
                let mut bundle = ZipArchive::new(File::open(&path).unwrap()).unwrap();
                let expected = render::render_note(&document, &mut bundle, format).unwrap();
                assert!(
                    memory.render(format).unwrap() == expected,
                    "{name}: {format:?}"
                );
            }
        }
    }

    #[test]
    fn rejects_unsupported_extensions_and_corrupt_archives() {
        assert!(Converter::new(b"abc".to_vec(), "test.txt")
            .err()
            .unwrap()
            .to_string()
            .contains(".note or .ntb"));
        assert!(Converter::new(b"abc".to_vec(), "test.ntb").is_err());
        assert!(Converter::new(b"abc".to_vec(), "test.note").is_err());
    }
}
