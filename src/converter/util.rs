use super::model::{DEFAULT_EXPORT_WIDTH_PT, NoteDocument};
use plist::Value;

pub(crate) fn parse_pair(value: &str) -> Option<(f32, f32)> {
    let numbers = parse_numbers(value);
    (numbers.len() >= 2).then(|| (numbers[0], numbers[1]))
}

pub(crate) fn parse_rect(value: &str) -> Option<(f32, f32, f32, f32)> {
    let numbers = parse_numbers(value);
    (numbers.len() >= 4).then(|| (numbers[0], numbers[1], numbers[2], numbers[3]))
}

fn parse_numbers(value: &str) -> Vec<f32> {
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
    numbers
}

pub(crate) fn value_f32(value: &Value) -> Option<f32> {
    match value {
        Value::Real(value) => Some(*value as f32),
        Value::Integer(value) => value
            .as_signed()
            .map(|v| v as f32)
            .or_else(|| value.as_unsigned().map(|v| v as f32)),
        _ => None,
    }
}

pub(crate) fn value_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(value) => value
            .as_signed()
            .or_else(|| value.as_unsigned().map(|v| v as i64)),
        Value::Real(value) => Some(*value as i64),
        _ => None,
    }
}

pub(crate) fn parse_line_spacing_doc(
    line_style: Option<&str>,
    page_width_doc: f32,
    export_width_pt: f32,
) -> f32 {
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

pub(crate) fn choose_export_width(paper_size: Option<&str>) -> f32 {
    match paper_size.map(str::to_ascii_lowercase).as_deref() {
        Some("a4") => 595.2756,
        Some("letter" | "legal") => 612.0,
        _ => DEFAULT_EXPORT_WIDTH_PT,
    }
}

pub(crate) fn page_count_for(note: &NoteDocument) -> usize {
    let mut page_count = 1usize;
    if let Some(max_page) = note.pdf_pages.iter().map(|page| page.page_index).max() {
        page_count = page_count.max(max_page + 1);
    }
    if let Some(max_page) = note.media_images.iter().map(|media| media.page_index).max() {
        page_count = page_count.max(max_page + 1);
    }
    if let Some(max_page) = note.text_blocks.iter().map(|block| block.page_index).max() {
        page_count = page_count.max(max_page + 1);
    }
    if let Some(max_page) = note.curves.iter().map(|curve| curve.page_index).max() {
        page_count = page_count.max(max_page + 1);
    }
    page_count
}
