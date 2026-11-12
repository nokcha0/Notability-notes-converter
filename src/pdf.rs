use crate::Result;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

pub fn page_id_by_index(doc: &Document, zero_based_index: usize) -> Result<ObjectId> {
    doc.get_pages()
        .get(&((zero_based_index + 1) as u32))
        .copied()
        .ok_or_else(|| format!("PDF page index {zero_based_index} not found").into())
}

pub fn add_form_xobject(
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

pub fn page_box(doc: &Document, page_id: ObjectId) -> Result<[f32; 4]> {
    let object = inherited_page_object(doc, page_id, b"CropBox")
        .or_else(|| inherited_page_object(doc, page_id, b"MediaBox"))
        .ok_or("page has no MediaBox")?;
    let values = doc.dereference(&object)?.1.as_array()?;
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

pub(crate) fn page_display_size(doc: &Document, page_id: ObjectId) -> Result<(f32, f32)> {
    let bounds = page_box(doc, page_id)?;
    let width = bounds[2] - bounds[0];
    let height = bounds[3] - bounds[1];
    if !bounds.iter().all(|n| n.is_finite()) || width <= 0.0 || height <= 0.0 {
        return Err("PDF page has invalid bounds".into());
    }
    Ok(if matches!(page_rotation(doc, page_id), 90 | 270) {
        (height, width)
    } else {
        (width, height)
    })
}

fn page_rotation(doc: &Document, page_id: ObjectId) -> i64 {
    inherited_page_object(doc, page_id, b"Rotate")
        .and_then(|value| {
            doc.dereference(&value)
                .ok()
                .and_then(|(_, object)| object.as_i64().ok())
        })
        .unwrap_or(0)
        .rem_euclid(360)
}

pub(crate) fn page_render_transform(
    doc: &Document,
    page_id: ObjectId,
    bbox: [f32; 4],
    width: f32,
    height: f32,
) -> Result<[f32; 6]> {
    let [x0, y0, x1, y1] = bbox;
    let source_width = x1 - x0;
    let source_height = y1 - y0;
    if !bbox.iter().all(|v| v.is_finite()) || source_width <= 0.0 || source_height <= 0.0 {
        return Err("PDF page has invalid bounds".into());
    }
    let rotation = page_rotation(doc, page_id);
    let (sx, sy) = if matches!(rotation, 90 | 270) {
        (width / source_height, height / source_width)
    } else {
        (width / source_width, height / source_height)
    };
    Ok(match rotation {
        90 => [0.0, -sy, sx, 0.0, -y0 * sx, x1 * sy],
        180 => [-sx, 0.0, 0.0, -sy, x1 * sx, y1 * sy],
        270 => [0.0, sy, -sx, 0.0, y1 * sx, -x0 * sy],
        _ => [sx, 0.0, 0.0, sy, -x0 * sx, -y0 * sy],
    })
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

#[cfg(test)]
mod tests {
    use super::{page_box, page_display_size, page_render_transform};
    use lopdf::{dictionary, Document};

    #[test]
    fn honors_inherited_crop_and_rotation_on_imported_pages() {
        let mut pdf = Document::with_version("1.5");
        let parent = pdf.add_object(dictionary! {
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 400.into()],
            "CropBox" => vec![10.into(), 20.into(), 110.into(), 120.into()],
            "Rotate" => 90,
        });
        let page = pdf.add_object(dictionary! { "Parent" => parent });
        let bbox = page_box(&pdf, page).unwrap();
        assert_eq!(bbox, [10.0, 20.0, 110.0, 120.0]);
        let transform = page_render_transform(&pdf, page, bbox, 200.0, 400.0).unwrap();
        assert_eq!(transform, [0.0, -4.0, 2.0, 0.0, -40.0, 440.0]);
        // Visible bottom-left becomes top-left after a clockwise quarter-turn.
        let [a, b, c, d, x, y] = transform;
        assert_eq!(
            (a * 10.0 + c * 20.0 + x, b * 10.0 + d * 20.0 + y),
            (0.0, 400.0)
        );
        assert_eq!(
            (a * 110.0 + c * 120.0 + x, b * 110.0 + d * 120.0 + y),
            (200.0, 0.0)
        );
    }

    #[test]
    fn rotated_background_dimensions_use_the_visible_page_box() {
        let mut pdf = Document::with_version("1.5");
        let crop = pdf.add_object(vec![10.into(), 20.into(), 190.into(), 280.into()]);
        let rotation = pdf.add_object(-270);
        let parent = pdf.add_object(dictionary! { "CropBox" => crop, "Rotate" => rotation });
        let page = pdf.add_object(dictionary! { "Parent" => parent });
        assert_eq!(page_box(&pdf, page).unwrap(), [10., 20., 190., 280.]);
        assert_eq!(page_display_size(&pdf, page).unwrap(), (260., 180.));
        let transform =
            page_render_transform(&pdf, page, page_box(&pdf, page).unwrap(), 260., 180.).unwrap();
        assert_eq!(transform, [0., -1., 1., 0., -20., 190.]);
    }
}
