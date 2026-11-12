//! Event-linked text containers and imported document assets.
use super::{
    build_text_spans, page_index, parse_paragraph_style_event, parse_text_style_event,
    unrotate_top_left, FlatBuffer, TextSpan, TextStyle, EVENT_DOCUMENT, EVENT_MEDIA,
    EVENT_PARAGRAPH_STYLE, EVENT_TEXT, EVENT_TEXT_STYLE,
};
use crate::converter::model::{EmbeddedPdfPage, TextBlock};
use crate::converter::note::read_zip_entry;
use crate::Result;
use lopdf::Document;
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use zip::ZipArchive;

type EventId = [u32; 2];
const EVENT_TEXT_BOUNDARY: u8 = 7;

fn event_id(flat: &FlatBuffer<'_>, table: usize, slot: usize) -> Option<EventId> {
    let field = flat.field(table, slot)?;
    Some([flat.u32(field)?, flat.u32(field + 4)?])
}

fn event_type(flat: &FlatBuffer<'_>, event: usize) -> Option<u8> {
    flat.field(event, 4).and_then(|field| flat.u8(field))
}

#[derive(Default)]
struct TextStore {
    text: String,
    utf16_length: usize,
    styles: Vec<super::TextStyleEvent>,
    paragraphs: Vec<super::ParagraphStyleEvent>,
}

#[derive(Clone, Copy)]
struct TextPosition {
    owner: EventId,
    offset: usize,
}

fn text_position(
    flat: &FlatBuffer<'_>,
    payload: usize,
    slot: usize,
    anchors: &BTreeMap<EventId, TextPosition>,
) -> Option<TextPosition> {
    let anchor = *anchors.get(&event_id(flat, payload, slot)?)?;
    let field = flat.field(payload, slot)?;
    Some(TextPosition {
        owner: anchor.owner,
        offset: anchor.offset.checked_add(flat.u32(field + 8)? as usize)?,
    })
}

// Text positions use UTF-16 offsets, whereas the renderer indexes Unicode characters.
fn character_offset(text: &str, utf16_offset: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.chars().enumerate() {
        if units >= utf16_offset {
            return index;
        }
        units += ch.len_utf16();
    }
    text.chars().count()
}

pub(super) fn reconstruct_text(
    flat: &FlatBuffer<'_>,
    events: &[usize],
    default_style: TextStyle,
    inset: f32,
) -> Result<(String, Vec<TextSpan>, Vec<TextBlock>)> {
    let document_id = events
        .iter()
        .find_map(|event| {
            (event_type(flat, *event) == Some(EVENT_DOCUMENT))
                .then(|| event_id(flat, *event, 0))
                .flatten()
        })
        .unwrap_or([0, 0]);
    let mut stores = BTreeMap::<EventId, TextStore>::new();
    let mut anchors = BTreeMap::<EventId, TextPosition>::new();
    anchors.insert(
        document_id,
        TextPosition {
            owner: document_id,
            offset: 0,
        },
    );

    for &event in events {
        if event_type(flat, event) != Some(EVENT_TEXT) {
            continue;
        }
        let Some(payload) = flat.offset_field(event, 5) else {
            continue;
        };
        let Some(id) = event_id(flat, event, 0) else {
            continue;
        };
        let Some(text) = flat.string_field(payload, 1) else {
            continue;
        };
        let owner = event_id(flat, payload, 0).unwrap_or(document_id);
        let store = stores.entry(owner).or_default();
        anchors.insert(
            id,
            TextPosition {
                owner,
                offset: store.utf16_length,
            },
        );
        store.text.push_str(text);
        store.utf16_length += text.encode_utf16().count();
    }
    // Boundary events provide end anchors such as the main text's (0, 3).
    for &event in events {
        if event_type(flat, event) != Some(EVENT_TEXT_BOUNDARY) {
            continue;
        }
        let Some(payload) = flat.offset_field(event, 5) else {
            continue;
        };
        if let (Some(id), Some(position)) = (
            event_id(flat, event, 0),
            text_position(flat, payload, 0, &anchors),
        ) {
            anchors.insert(id, position);
        }
    }
    for &event in events {
        let Some(kind @ (EVENT_TEXT_STYLE | EVENT_PARAGRAPH_STYLE)) = event_type(flat, event)
        else {
            continue;
        };
        let Some(payload) = flat.offset_field(event, 5) else {
            continue;
        };
        let start = text_position(flat, payload, 0, &anchors);
        let end = text_position(flat, payload, 1, &anchors);
        let Some(owner) = end.or(start).map(|position| position.owner) else {
            continue;
        };
        // A document-start sentinel can start the first range of a positioned store.
        if start.is_some_and(|p| p.owner != owner && p.owner != document_id) {
            continue;
        }
        let Some(store) = stores.get_mut(&owner) else {
            continue;
        };
        let start = character_offset(&store.text, start.map_or(0, |p| p.offset));
        let end = character_offset(&store.text, end.map_or(usize::MAX, |p| p.offset));
        if kind == EVENT_TEXT_STYLE {
            let mut style = parse_text_style_event(flat, payload);
            style.start = start;
            style.end = end;
            store.styles.push(style);
        } else {
            let mut paragraph = parse_paragraph_style_event(flat, payload);
            paragraph.start = start;
            paragraph.end = end;
            store.paragraphs.push(paragraph);
        }
    }

    let mut blocks = Vec::new();
    for (index, &event) in events.iter().enumerate() {
        if event_type(flat, event) != Some(EVENT_MEDIA) {
            continue;
        }
        let Some(id) = event_id(flat, event, 0) else {
            continue;
        };
        let Some(store) = stores.remove(&id) else {
            continue;
        };
        let payload = flat
            .offset_field(event, 5)
            .ok_or("NTB text box has no payload")?;
        let (x, y) = flat
            .pair_field(payload, 3)
            .ok_or("NTB text box has no position")?;
        let (width, height) = flat
            .pair_field(payload, 6)
            .ok_or("NTB text box has no size")?;
        if ![x, y, width, height].iter().all(|v| v.is_finite()) || width <= 0.0 || height <= 0.0 {
            return Err("NTB text box has invalid geometry".into());
        }
        let rotation = flat
            .field(payload, 4)
            .and_then(|f| flat.f32(f))
            .unwrap_or(0.0);
        let (x, y) = unrotate_top_left((x - inset, y), (width, height), rotation);
        let text_spans = build_text_spans(
            &store.text,
            default_style.clone(),
            &store.styles,
            &store.paragraphs,
        );
        blocks.push(TextBlock {
            page_index: page_index(flat, payload, 2),
            x,
            y,
            width,
            height,
            rotation_degrees: rotation.to_degrees(),
            text: store.text,
            text_spans,
            z_index: flat
                .field(payload, 9)
                .and_then(|f| flat.u64(f))
                .and_then(|v| i64::try_from(v).ok())
                .unwrap_or(index as i64),
        });
    }
    let main = stores.remove(&document_id).unwrap_or_default();
    if stores.values().any(|store| !store.text.is_empty()) {
        return Err("NTB contains text whose container could not be resolved".into());
    }
    let spans = build_text_spans(&main.text, default_style, &main.styles, &main.paragraphs);
    blocks.sort_by_key(|block| (block.page_index, block.z_index));
    Ok((main.text, spans, blocks))
}

/// A single imported PDF maps source page numbers directly to note page numbers.
/// Preserve pages without annotations as well. Multiple assets need explicit page
/// placement metadata; choosing an arbitrary archive entry would lose content.
pub(super) fn pdf_backgrounds<R: Read + Seek>(
    bundle: &mut ZipArchive<R>,
) -> Result<Vec<EmbeddedPdfPage>> {
    let pdfs: Vec<String> = bundle
        .file_names()
        .filter(|name| name.starts_with("assets/") && name.to_ascii_lowercase().ends_with(".pdf"))
        .map(str::to_owned)
        .collect();
    if pdfs.len() > 1 {
        return Err(
            "NTB contains multiple PDF assets; their page placement is not yet supported".into(),
        );
    }
    let Some(path) = pdfs.first() else {
        return Ok(Vec::new());
    };
    let bytes = read_zip_entry(bundle, path)?;
    let pdf = Document::load_mem(&bytes)?;
    Ok((0..pdf.get_pages().len())
        .map(|page| EmbeddedPdfPage {
            page_index: page,
            relative_path: path.clone(),
            source_page_index: page,
        })
        .collect())
}
