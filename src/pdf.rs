use crate::Result;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

pub(crate) fn page_id_by_index(doc: &Document, zero_based_index: usize) -> Result<ObjectId> {
    doc.get_pages()
        .get(&((zero_based_index + 1) as u32))
        .copied()
        .ok_or_else(|| format!("PDF page index {zero_based_index} not found").into())
}

pub(crate) fn add_form_xobject(
    output: &mut Document,
    source_doc: &Document,
    page_id: ObjectId,
    bbox: [f32; 4],
) -> Result<ObjectId> {
    let content = source_doc.get_page_content(page_id)?;
    let resources = inherited_page_object(source_doc, page_id, b"Resources")
        .unwrap_or_else(|| Dictionary::new().into());
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

pub(crate) fn page_box(doc: &Document, page_id: ObjectId) -> Result<[f32; 4]> {
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
