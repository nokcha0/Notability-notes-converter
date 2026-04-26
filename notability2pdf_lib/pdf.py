from __future__ import annotations

from pathlib import Path

from PIL import Image


def write_pdf(
    pages: list[Image.Image],
    output_path: Path,
    export_width_pt: float,
    export_height_pt: float,
) -> None:
    if not pages:
        raise ValueError("No rendered pages to write")

    output_path.parent.mkdir(parents=True, exist_ok=True)
    first_page, *rest = pages
    x_dpi = first_page.width * 72.0 / export_width_pt
    y_dpi = first_page.height * 72.0 / export_height_pt
    first_page.save(
        output_path,
        save_all=True,
        append_images=rest,
        dpi=(x_dpi, y_dpi),
        quality=100,
        subsampling=0,
    )
