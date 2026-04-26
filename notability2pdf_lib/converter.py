from __future__ import annotations

import shutil
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


def iter_source_files(source_dir: Path, output_dir: Path) -> list[Path]:
    output_root = output_dir.resolve()
    return sorted(
        path
        for path in source_dir.rglob("*")
        if path.is_file() and not is_relative_to(path.resolve(), output_root)
    )


def output_path_for_note(source_dir: Path, note_path: Path, output_dir: Path) -> Path:
    return output_dir / note_path.relative_to(source_dir).with_suffix(".pdf")


def output_path_for_source_file(source_dir: Path, source_path: Path, output_dir: Path) -> Path:
    if source_path.suffix.lower() == ".note":
        return output_path_for_note(source_dir, source_path, output_dir)
    return output_dir / source_path.relative_to(source_dir)


def validate_output_paths(source_dir: Path, source_files: list[Path], output_dir: Path) -> None:
    target_sources: dict[Path, Path] = {}
    for source_path in source_files:
        output_path = output_path_for_source_file(source_dir, source_path, output_dir)
        previous_source = target_sources.get(output_path)
        if previous_source is not None:
            raise FileExistsError(
                "Output path collision: "
                f"{previous_source.relative_to(source_dir)} and {source_path.relative_to(source_dir)} "
                f"both map to {output_path.relative_to(output_dir)}"
            )
        target_sources[output_path] = source_path


def prepare_output_dir(source_dir: Path, output_dir: Path) -> None:
    source_root = source_dir.resolve()
    output_root = output_dir.resolve()
    if source_root == output_root:
        raise ValueError("Output folder must be different from input folder")
    if is_relative_to(source_root, output_root):
        raise ValueError("Output folder must not contain the input folder")
    if output_dir.exists():
        if not output_dir.is_dir():
            raise ValueError(f"Output path exists and is not a folder: {output_dir}")
        shutil.rmtree(output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)


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
    source_files = iter_source_files(source_dir, output_dir)
    validate_output_paths(source_dir, source_files, output_dir)
    prepare_output_dir(source_dir, output_dir)
    output_root = output_dir.resolve()
    for source_subdir in sorted(path for path in source_dir.rglob("*") if path.is_dir()):
        if is_relative_to(source_subdir.resolve(), output_root):
            continue
        (output_dir / source_subdir.relative_to(source_dir)).mkdir(parents=True, exist_ok=True)

    written_paths: list[Path] = []
    for source_path in source_files:
        output_path = output_path_for_source_file(source_dir, source_path, output_dir)
        if source_path.suffix.lower() != ".note":
            output_path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source_path, output_path)
            written_paths.append(output_path)
            continue

        svg_dir = None
        if keep_svg:
            svg_dir = output_dir / "svg" / source_path.relative_to(source_dir).with_suffix("")
        convert_note_to_pdf(source_path, output_path, render_scale, raster=raster, svg_dir=svg_dir)
        written_paths.append(output_path)
    return written_paths
