//! Bindings for the same engine used by the CLI.
use notability_converter::{OutputFormat, PdfBackgrounds};
use std::io::{Cursor, Write};
use wasm_bindgen::prelude::*;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

mod input_archive;

#[wasm_bindgen]
pub struct InputArchive {
    inner: input_archive::InputZip,
}

#[wasm_bindgen]
impl InputArchive {
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: Vec<u8>) -> Result<InputArchive, JsError> {
        Ok(Self {
            inner: input_archive::InputZip::new(bytes).map_err(js_error)?,
        })
    }

    pub fn names(&self) -> Vec<String> {
        self.inner.names()
    }

    pub fn converter(&mut self, index: usize) -> Result<Converter, JsError> {
        let (bytes, name) = self.inner.read(index).map_err(js_error)?;
        Converter::new(bytes, &name)
    }
}

fn js_error(error: impl std::fmt::Display) -> JsError {
    JsError::new(&error.to_string())
}

#[wasm_bindgen]
pub struct Converter {
    inner: notability_converter::Converter,
    pdf_backgrounds: PdfBackgrounds,
    rendered_backgrounds: Vec<Vec<u8>>,
}

#[wasm_bindgen]
impl Converter {
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: Vec<u8>, name: &str) -> Result<Converter, JsError> {
        Ok(Self {
            inner: notability_converter::Converter::new(bytes, name).map_err(js_error)?,
            pdf_backgrounds: PdfBackgrounds::default(),
            rendered_backgrounds: Vec::new(),
        })
    }

    pub fn pdf(&self) -> Result<Vec<u8>, JsError> {
        self.inner.render(OutputFormat::Pdf).map_err(js_error)
    }

    /// PDF.js renders these pages to PNG before SVG output is requested.
    pub fn prepare_svg(&mut self) -> Result<usize, JsError> {
        self.pdf_backgrounds = self.inner.pdf_backgrounds().map_err(js_error)?;
        self.rendered_backgrounds = vec![Vec::new(); self.pdf_backgrounds.pages.len()];
        Ok(self.pdf_backgrounds.pages.len())
    }

    pub fn background_asset_count(&self) -> usize {
        self.pdf_backgrounds.assets.len()
    }

    pub fn background_pdf(&self, index: usize) -> Result<Vec<u8>, JsError> {
        self.pdf_backgrounds
            .assets
            .get(index)
            .cloned()
            .ok_or_else(|| JsError::new("Invalid PDF background index"))
    }

    pub fn background_asset(&self, index: usize) -> Result<usize, JsError> {
        self.pdf_backgrounds
            .pages
            .get(index)
            .map(|page| page.asset_index)
            .ok_or_else(|| JsError::new("Invalid PDF background index"))
    }

    pub fn background_page(&self, index: usize) -> Result<usize, JsError> {
        self.pdf_backgrounds
            .pages
            .get(index)
            .map(|page| page.page_index)
            .ok_or_else(|| JsError::new("Invalid PDF background index"))
    }

    pub fn set_background(&mut self, index: usize, png: Vec<u8>) -> Result<(), JsError> {
        let destination = self
            .rendered_backgrounds
            .get_mut(index)
            .ok_or_else(|| JsError::new("Invalid PDF background index"))?;
        *destination = png;
        Ok(())
    }

    pub fn svg(&self) -> Result<Vec<u8>, JsError> {
        self.inner
            .render_svg_with_backgrounds(&self.rendered_backgrounds)
            .map_err(js_error)
    }
}

#[wasm_bindgen]
pub struct Archive {
    writer: ZipWriter<Cursor<Vec<u8>>>,
}

#[wasm_bindgen]
impl Archive {
    #[wasm_bindgen(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            writer: ZipWriter::new(Cursor::new(Vec::new())),
        }
    }

    pub fn add(&mut self, name: &str, bytes: &[u8]) -> Result<(), JsError> {
        if name.is_empty()
            || name.starts_with('/')
            || name.contains(['\\', ':', '\0'])
            || name
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(JsError::new("Invalid download name"));
        }
        self.writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .map_err(js_error)?;
        self.writer.write_all(bytes).map_err(js_error)
    }

    pub fn finish(self) -> Result<Vec<u8>, JsError> {
        Ok(self.writer.finish().map_err(js_error)?.into_inner())
    }
}
