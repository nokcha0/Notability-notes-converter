pub(crate) const DEFAULT_TEXT_TOP_DOC: f32 = 3.0;
pub(crate) const DEFAULT_EXPORT_WIDTH_PT: f32 = 612.0;
pub(crate) const DEFAULT_PAGE_RATIO: f32 = 21.0 / 16.0;
pub(crate) const DEFAULT_CONTENT_INSET_RATIO: f32 = 1.0 / 38.4;
pub(crate) const DEFAULT_INPUT_DIR: &str = "input";
pub(crate) const DEFAULT_OUTPUT_DIR: &str = "output";
pub(crate) const STROKE_STYLE_HIGHLIGHTER: u8 = 4;
pub(crate) const STROKE_STYLE_PENCIL: u8 = 5;
pub(crate) const STROKE_STYLE_NOT_EXPORTED: u8 = 6;
pub(crate) const CURVE_SMOOTHING_TENSION: f32 = 0.8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputFormat {
    Pdf,
    Svg,
}

impl OutputFormat {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Svg => "svg",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TextStyle {
    pub(crate) font_size: f32,
    pub(crate) font_name: String,
    pub(crate) line_spacing_multiplier: f32,
    pub(crate) color: [u8; 4],
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) strikethrough: bool,
    pub(crate) baseline_offset: f32,
    pub(crate) indent_level: Option<usize>,
    pub(crate) indent_decoration_style: Option<i64>,
    pub(crate) indent_decoration_number: Option<i64>,
    pub(crate) checklist_checked: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct TextSpan {
    pub(crate) start: usize,
    pub(crate) length: usize,
    pub(crate) style: TextStyle,
}

#[derive(Clone, Debug)]
pub(crate) struct TextBlock {
    pub(crate) page_index: usize,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) text: String,
    pub(crate) text_spans: Vec<TextSpan>,
    pub(crate) z_index: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct MediaImage {
    pub(crate) page_index: usize,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) rotation_degrees: f32,
    pub(crate) flipped_horizontal: bool,
    pub(crate) flipped_vertical: bool,
    pub(crate) relative_path: String,
    pub(crate) z_index: i64,
    pub(crate) crop: Option<ImageCrop>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ImageCrop {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Debug)]
pub(crate) struct StrokeCurve {
    pub(crate) page_index: usize,
    pub(crate) points: Vec<(f32, f32)>,
    pub(crate) preserve_vertices: bool,
    pub(crate) width: f32,
    pub(crate) rgba: [u8; 4],
    pub(crate) style: u8,
    pub(crate) dash_pattern: Option<u8>,
    pub(crate) pressures: Vec<f32>,
    pub(crate) fractional_widths: Vec<f32>,
}

#[derive(Clone, Debug)]
pub(crate) struct EmbeddedPdfPage {
    pub(crate) page_index: usize,
    pub(crate) relative_path: String,
    pub(crate) source_page_index: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct NoteDocument {
    pub(crate) bundle_root: String,
    pub(crate) page_width_doc: f32,
    pub(crate) page_height_doc: f32,
    pub(crate) export_width_pt: f32,
    pub(crate) export_height_pt: f32,
    pub(crate) line_style: Option<String>,
    pub(crate) text: String,
    pub(crate) text_spans: Vec<TextSpan>,
    pub(crate) text_blocks: Vec<TextBlock>,
    pub(crate) media_images: Vec<MediaImage>,
    pub(crate) pdf_pages: Vec<EmbeddedPdfPage>,
    pub(crate) curves: Vec<StrokeCurve>,
}
