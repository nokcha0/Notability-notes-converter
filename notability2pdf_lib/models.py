from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class TextStyle:
    font_name: str
    font_size: float
    color: tuple[int, int, int, int]


@dataclass(frozen=True)
class TextSpan:
    start: int
    length: int
    style: TextStyle


@dataclass(frozen=True)
class MediaImage:
    page_index: int
    x: float
    y: float
    width: float
    height: float
    relative_path: str
    z_index: int


@dataclass(frozen=True)
class StrokeCurve:
    page_index: int
    points: tuple[tuple[float, float], ...]
    width: float
    rgba: tuple[int, int, int, int]
    style: int
    pressures: tuple[float, ...]
    fractional_widths: tuple[float, ...]


@dataclass(frozen=True)
class EmbeddedPdfPage:
    page_index: int
    relative_path: str
    source_page_index: int


@dataclass(frozen=True)
class NoteDocument:
    bundle_root: str
    page_width_doc: float
    page_height_doc: float
    export_width_pt: float
    export_height_pt: float
    line_style: str | None
    text: str
    text_spans: tuple[TextSpan, ...]
    media_images: tuple[MediaImage, ...]
    pdf_pages: tuple[EmbeddedPdfPage, ...]
    curves: tuple[StrokeCurve, ...]
