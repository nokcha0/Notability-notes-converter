#[macro_use]
extern crate lopdf;

mod converter;
mod pdf;

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
use crate::pdf::{add_form_xobject, page_box, page_id_by_index};

#[derive(Debug, Deserialize)]
struct Manifest {
    output: PathBuf,
    overlay: PathBuf,
    width: f32,
    height: f32,
    pages: Vec<PageSpec>,
}

#[derive(Debug, Deserialize)]
struct PageSpec {
    base_pdf: Option<PathBuf>,
    source_page_index: Option<usize>,
}

fn main() {
    if let Err(error) = dispatch() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn dispatch() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() == 1 && args[0].ends_with(".json") {
        run_pdfmerge(Path::new(&args[0]))
    } else {
        converter::run_cli(args)
    }
}

fn run_pdfmerge(manifest_path: &Path) -> Result<()> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path)?)?;
    let mut output = Document::with_version("1.5");
    let mut loaded_docs: BTreeMap<PathBuf, Document> = BTreeMap::new();
    let mut max_id = 1;

    load_pdf_into_output(&manifest.overlay, &mut output, &mut loaded_docs, &mut max_id)?;
    for page in &manifest.pages {
        if let Some(base_pdf) = &page.base_pdf {
            load_pdf_into_output(base_pdf, &mut output, &mut loaded_docs, &mut max_id)?;
        }
    }
    output.max_id = max_id.saturating_sub(1);

    let pages_id = output.new_object_id();
    let mut page_ids = Vec::with_capacity(manifest.pages.len());
    for (page_index, page_spec) in manifest.pages.iter().enumerate() {
        let page_id = add_composed_page(
            &mut output,
            &loaded_docs,
            &manifest,
            page_spec,
            page_index,
            pages_id,
        )?;
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
    output.save(&manifest.output)?;
    Ok(())
}

fn load_pdf_into_output(
    path: &Path,
    output: &mut Document,
    loaded_docs: &mut BTreeMap<PathBuf, Document>,
    max_id: &mut u32,
) -> Result<()> {
    if loaded_docs.contains_key(path) {
        return Ok(());
    }
    let mut doc = Document::load(path)?;
    doc.renumber_objects_with(*max_id);
    *max_id = doc.max_id + 1;
    for (object_id, object) in doc.objects.iter() {
        output.objects.insert(*object_id, object.clone());
    }
    loaded_docs.insert(path.to_path_buf(), doc);
    Ok(())
}

fn add_composed_page(
    output: &mut Document,
    loaded_docs: &BTreeMap<PathBuf, Document>,
    manifest: &Manifest,
    page_spec: &PageSpec,
    page_index: usize,
    pages_id: ObjectId,
) -> Result<ObjectId> {
    let mut xobjects = Dictionary::new();
    let mut operations = Vec::new();

    if let Some(base_pdf) = &page_spec.base_pdf {
        let source_page_index = page_spec.source_page_index.unwrap_or(0);
        let base_doc = loaded_docs
            .get(base_pdf)
            .ok_or("base PDF missing from loaded docs")?;
        let base_page_id = page_id_by_index(base_doc, source_page_index)?;
        let base_box = page_box(base_doc, base_page_id)?;
        let base_form_id = add_form_xobject(output, base_doc, base_page_id, base_box)?;
        xobjects.set("Base", base_form_id);
        let width = base_box[2] - base_box[0];
        let height = base_box[3] - base_box[1];
        let sx = manifest.width / width;
        let sy = manifest.height / height;
        operations.extend([
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![
                    sx.into(),
                    0.into(),
                    0.into(),
                    sy.into(),
                    (-base_box[0] * sx).into(),
                    (-base_box[1] * sy).into(),
                ],
            ),
            Operation::new("Do", vec!["Base".into()]),
            Operation::new("Q", vec![]),
        ]);
    }

    let overlay_doc = loaded_docs
        .get(&manifest.overlay)
        .ok_or("overlay PDF missing from loaded docs")?;
    let overlay_page_id = page_id_by_index(overlay_doc, page_index)?;
    let overlay_box = page_box(overlay_doc, overlay_page_id)?;
    let overlay_form_id = add_form_xobject(output, overlay_doc, overlay_page_id, overlay_box)?;
    xobjects.set("Overlay", overlay_form_id);
    operations.extend([
        Operation::new("q", vec![]),
        Operation::new("Do", vec!["Overlay".into()]),
        Operation::new("Q", vec![]),
    ]);

    let resources_id = output.add_object(dictionary! {
        "XObject" => xobjects,
    });
    let content = Content { operations }.encode()?;
    let content_id = output.add_object(Stream::new(Dictionary::new(), content));
    let page_id = output.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), manifest.width.into(), manifest.height.into()],
    });
    Ok(page_id)
}
