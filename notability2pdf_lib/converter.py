from __future__ import annotations

from pathlib import Path

from .parser import load_note_document
from .pdf import write_pdf
from .rendering import render_note_pages
from .vector_pdf import write_vector_pdf


def is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
    except ValueError:
        return False
    return True


def iter_note_files(source_dir: Path) -> list[Path]:
    return sorted(path for path in source_dir.rglob("*") if path.is_file() and path.suffix.lower() == ".note")


def output_path_for_note(source_dir: Path, note_path: Path, output_dir: Path) -> Path:
    return output_dir / note_path.relative_to(source_dir).with_suffix(".pdf")


def convert_note_to_raster_pdf(note_path: Path, output_path: Path, render_scale: float) -> None:
    note = load_note_document(note_path)
    pages = render_note_pages(note, note_path, render_scale)
    write_pdf(pages, output_path, note.export_width_pt, note.export_height_pt)


def convert_note_to_pdf(
    note_path: Path,
    output_path: Path,
    render_scale: float = 2.0,
    *,
    raster: bool = False,
    svg_dir: Path | None = None,
) -> None:
    note = load_note_document(note_path)
    if raster:
        pages = render_note_pages(note, note_path, render_scale)
        write_pdf(pages, output_path, note.export_width_pt, note.export_height_pt)
    else:
        write_vector_pdf(note, note_path, output_path, svg_dir=svg_dir)


def convert_note_folder_to_pdfs(
    source_dir: Path,
    output_dir: Path,
    render_scale: float = 2.0,
    *,
    raster: bool = False,
    keep_svg: bool = False,
) -> list[Path]:
    output_dir.mkdir(parents=True, exist_ok=True)
    output_root = output_dir.resolve()
    for source_subdir in sorted(path for path in source_dir.rglob("*") if path.is_dir()):
        if is_relative_to(source_subdir.resolve(), output_root):
            continue
        (output_dir / source_subdir.relative_to(source_dir)).mkdir(parents=True, exist_ok=True)

    written_paths: list[Path] = []
    for note_path in iter_note_files(source_dir):
        if is_relative_to(note_path.resolve(), output_root):
            continue
        output_path = output_path_for_note(source_dir, note_path, output_dir)
        svg_dir = None
        if keep_svg:
            svg_dir = output_dir / "svg" / note_path.relative_to(source_dir).with_suffix("")
        convert_note_to_pdf(note_path, output_path, render_scale, raster=raster, svg_dir=svg_dir)
        written_paths.append(output_path)
    return written_paths
