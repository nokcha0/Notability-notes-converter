from __future__ import annotations

import math
import plistlib
import re
import struct
import zipfile
import logging
from io import BytesIO
from pathlib import Path
from typing import Any

from PIL import Image
from pypdf import PdfReader

from .archive import KeyedArchive
from .constants import (
    DEFAULT_CONTENT_INSET_RATIO,
    DEFAULT_EXPORT_WIDTH_PT,
    DEFAULT_PAGE_RATIO,
    DEFAULT_TEXT_COLOR,
    EXPORT_WIDTHS_PT,
    STROKE_STYLE_PEN,
    STROKE_STYLE_PENCIL,
)
from .models import EmbeddedPdfPage, MediaImage, NoteDocument, StrokeCurve, TextSpan, TextStyle


PAIR_RE = re.compile(r"-?\d+(?:\.\d+)?")
logging.getLogger("pypdf").setLevel(logging.ERROR)


def parse_pair(value: str) -> tuple[float, float]:
    matches = PAIR_RE.findall(value)
    if len(matches) < 2:
        raise ValueError(f"Could not parse coordinate pair from {value!r}")
    return float(matches[0]), float(matches[1])


def parse_range(value: str) -> tuple[int, int]:
    start, length = parse_pair(value)
    return int(start), int(length)


def parse_text_color(raw: Any) -> tuple[int, int, int, int]:
    if isinstance(raw, str):
        parts = [float(part) for part in raw.split(",")]
        if len(parts) == 4:
            return tuple(max(0, min(255, round(part * 255))) for part in parts)  # type: ignore[return-value]
    if isinstance(raw, dict):
        red = float(raw.get("UIRed", raw.get("UIWhite", 0.0)))
        green = float(raw.get("UIGreen", raw.get("UIWhite", 0.0)))
        blue = float(raw.get("UIBlue", raw.get("UIWhite", 0.0)))
        alpha = float(raw.get("UIAlpha", 1.0))
        return (
            max(0, min(255, round(red * 255))),
            max(0, min(255, round(green * 255))),
            max(0, min(255, round(blue * 255))),
            max(0, min(255, round(alpha * 255))),
        )
    return DEFAULT_TEXT_COLOR


def parse_handwriting_color(raw: bytes) -> tuple[int, int, int, int]:
    if len(raw) != 4:
        return DEFAULT_TEXT_COLOR
    value = int.from_bytes(raw, "little")
    red = value & 0xFF
    green = (value >> 8) & 0xFF
    blue = (value >> 16) & 0xFF
    alpha = (value >> 24) & 0xFF
    return red, green, blue, alpha


def unpack_numeric_blob(blob: bytes, fmt: str) -> tuple[float, ...] | tuple[int, ...]:
    count = len(blob) // struct.calcsize(fmt)
    if count == 0:
        return ()
    return struct.unpack(f"<{count}{fmt}", blob)


def choose_export_width(paper_size: str | None) -> float:
    if not paper_size:
        return DEFAULT_EXPORT_WIDTH_PT
    return EXPORT_WIDTHS_PT.get(paper_size.lower(), DEFAULT_EXPORT_WIDTH_PT)


def find_bundle_root(entries: list[str]) -> str:
    for entry in entries:
        if entry.endswith("/Session.plist"):
            return entry[: -len("Session.plist")]
    raise ValueError("Could not find Session.plist inside the .note bundle")


def choose_thumbnail_entry(entries: list[str], bundle_root: str) -> str | None:
    thumb_entries = [
        entry
        for entry in entries
        if entry.startswith(bundle_root)
        and entry.lower().endswith(".png")
        and "thumb" in Path(entry).name.lower()
    ]
    if not thumb_entries:
        return None
    return max(thumb_entries, key=lambda item: Path(item).name.count("x"))


def parse_line_spacing_doc(line_style: str | None, page_width_doc: float, export_width_pt: float) -> float:
    if not line_style:
        return 20.0
    parts = line_style.split(":")
    try:
        spacing_inches = float(parts[-1])
    except ValueError:
        return 20.0
    doc_to_export = export_width_pt / page_width_doc
    return spacing_inches * 72.0 / doc_to_export


def parse_content_inset_doc(page_width_doc: float) -> float:
    return page_width_doc * DEFAULT_CONTENT_INSET_RATIO


def default_text_style() -> TextStyle:
    return TextStyle(font_name="HelveticaNeue", font_size=12.0, color=DEFAULT_TEXT_COLOR)


def resample_values(values: tuple[float, ...], target_count: int, default: float = 1.0) -> tuple[float, ...]:
    if target_count <= 0:
        return ()
    if not values:
        return tuple(default for _ in range(target_count))
    if len(values) == target_count:
        return values
    if len(values) == 1:
        return tuple(values[0] for _ in range(target_count))
    if target_count == 1:
        return (float(values[0]),)

    last_source_index = len(values) - 1
    last_target_index = target_count - 1
    result: list[float] = []
    for target_index in range(target_count):
        source_position = target_index * last_source_index / last_target_index
        lower = int(math.floor(source_position))
        upper = min(last_source_index, lower + 1)
        fraction = source_position - lower
        result.append(float(values[lower]) * (1.0 - fraction) + float(values[upper]) * fraction)
    return tuple(result)


def bezier_sample_count(point_count: int) -> int:
    if point_count > 0 and (point_count - 1) % 3 == 0:
        return ((point_count - 1) // 3) + 1
    return point_count


def is_bezier_point_count(point_count: int) -> bool:
    return point_count >= 4 and (point_count - 1) % 3 == 0


def parse_text(archive: KeyedArchive, rich_text: dict[str, Any]) -> tuple[str, tuple[TextSpan, ...]]:
    attributed = archive.ns_dict(rich_text.get("attributedString"))
    text = archive.as_text(attributed.get("stringKey", ""))
    spans: list[TextSpan] = []
    for raw_range in archive.ns_array(attributed.get("subRangesKey")):
        subrange = archive.ns_dict(raw_range)
        start, length = parse_range(archive.as_text(subrange.get("subRangeRangeKey", "{0, 0}")))
        font_attrs = archive.ns_dict(subrange.get("subRangeFontKey"))
        font_name = str(font_attrs.get("NSFontNameAttribute", "HelveticaNeue"))
        font_size = float(font_attrs.get("NSFontSizeAttribute", 12.0))
        color = parse_text_color(
            subrange.get("subRangeColorCrossPlatformKey")
            or archive.deref(subrange.get("subRangeColorKey"))
        )
        spans.append(TextSpan(start=start, length=length, style=TextStyle(font_name, font_size, color)))
    if not spans:
        spans.append(TextSpan(start=0, length=len(text), style=default_text_style()))
    return text, tuple(spans)


def parse_media_images(
    archive: KeyedArchive,
    rich_text: dict[str, Any],
    page_height_doc: float,
) -> tuple[MediaImage, ...]:
    images: list[MediaImage] = []
    for raw_media in archive.ns_array(rich_text.get("mediaObjects")):
        media = archive.deref(raw_media)
        if not isinstance(media, dict):
            continue
        try:
            x, y = parse_pair(archive.as_text(media["documentOrigin"]))
            width, height = parse_pair(archive.as_text(media["unscaledContentSize"]))
            figure = archive.deref(media["figure"])
            background = archive.deref(figure["FigureBackgroundObjectKey"])
            snapshot = archive.deref(background["kImageObjectSnapshotKey"])
            relative_path = archive.as_text(snapshot["relativePath"])
        except (KeyError, TypeError, ValueError):
            continue
        page_index = int(y // page_height_doc)
        local_y = y - page_index * page_height_doc
        images.append(
            MediaImage(
                page_index=page_index,
                x=x,
                y=local_y,
                width=width,
                height=height,
                relative_path=relative_path,
                z_index=int(media.get("zIndex", 0)),
            )
        )
    images.sort(key=lambda item: (item.page_index, item.z_index))
    return tuple(images)


def parse_pdf_pages(archive: KeyedArchive, rich_text: dict[str, Any]) -> tuple[EmbeddedPdfPage, ...]:
    pages: list[EmbeddedPdfPage] = []
    for raw_layout in archive.ns_array(rich_text.get("pageLayoutArray")):
        layout = archive.ns_dict(raw_layout)
        try:
            document_page = int(layout["kPageLayoutDocumentPageNumberKey"]) - 1
            source_page = int(layout["kPageLayoutPDFPageNumberKey"]) - 1
            filename = archive.as_text(layout["kPageLayoutPDFFileNameKey"])
        except (KeyError, TypeError, ValueError):
            continue
        pages.append(
            EmbeddedPdfPage(
                page_index=document_page,
                relative_path=f"PDFs/{filename}",
                source_page_index=source_page,
            )
        )
    pages.sort(key=lambda item: item.page_index)
    return tuple(pages)


def split_curve_into_pages(
    points: list[tuple[float, float]],
    width: float,
    rgba: tuple[int, int, int, int],
    style: int,
    pressures: tuple[float, ...],
    fractional_widths: tuple[float, ...],
    page_height_doc: float,
) -> list[StrokeCurve]:
    if not points:
        return []
    expected_sample_count = bezier_sample_count(len(points))
    if (
        is_bezier_point_count(len(points))
        and expected_sample_count == len(pressures) == len(fractional_widths)
    ):
        return split_bezier_curve_into_pages(
            points,
            width,
            rgba,
            style,
            pressures,
            fractional_widths,
            page_height_doc,
        )
    curves: list[StrokeCurve] = []
    current_page = int(points[0][1] // page_height_doc)
    current_points: list[tuple[float, float]] = []
    current_pressures: list[float] = []
    current_fractional_widths: list[float] = []
    for point_index, (x, y) in enumerate(points):
        page_index = int(y // page_height_doc)
        local_point = (x, y - page_index * page_height_doc)
        pressure = pressures[point_index] if point_index < len(pressures) else 1.0
        fractional_width = fractional_widths[point_index] if point_index < len(fractional_widths) else 1.0
        if page_index != current_page and current_points:
            curves.append(
                StrokeCurve(
                    page_index=current_page,
                    points=tuple(current_points),
                    width=width,
                    rgba=rgba,
                    style=style,
                    pressures=tuple(current_pressures),
                    fractional_widths=tuple(current_fractional_widths),
                )
            )
            current_points = [local_point]
            current_pressures = [pressure]
            current_fractional_widths = [fractional_width]
            current_page = page_index
        else:
            current_points.append(local_point)
            current_pressures.append(pressure)
            current_fractional_widths.append(fractional_width)
            current_page = page_index
    if current_points:
        curves.append(
            StrokeCurve(
                page_index=current_page,
                points=tuple(current_points),
                width=width,
                rgba=rgba,
                style=style,
                pressures=tuple(current_pressures),
                fractional_widths=tuple(current_fractional_widths),
            )
        )
    return curves


def split_bezier_curve_into_pages(
    points: list[tuple[float, float]],
    width: float,
    rgba: tuple[int, int, int, int],
    style: int,
    pressures: tuple[float, ...],
    fractional_widths: tuple[float, ...],
    page_height_doc: float,
) -> list[StrokeCurve]:
    curves: list[StrokeCurve] = []
    current_page: int | None = None
    current_points: list[tuple[float, float]] = []
    current_pressures: list[float] = []
    current_fractional_widths: list[float] = []

    def localize(point: tuple[float, float], page_index: int) -> tuple[float, float]:
        return point[0], point[1] - page_index * page_height_doc

    def flush() -> None:
        nonlocal current_points, current_pressures, current_fractional_widths, current_page
        if current_page is None or not current_points:
            return
        curves.append(
            StrokeCurve(
                page_index=current_page,
                points=tuple(current_points),
                width=width,
                rgba=rgba,
                style=style,
                pressures=tuple(current_pressures),
                fractional_widths=tuple(current_fractional_widths),
            )
        )
        current_points = []
        current_pressures = []
        current_fractional_widths = []
        current_page = None

    for segment_index in range((len(points) - 1) // 3):
        start = points[segment_index * 3]
        control_1 = points[segment_index * 3 + 1]
        control_2 = points[segment_index * 3 + 2]
        end = points[segment_index * 3 + 3]
        page_index = int(start[1] // page_height_doc)
        if current_page != page_index:
            flush()
            current_page = page_index
            current_points = [localize(start, page_index)]
            current_pressures = [pressures[segment_index]]
            current_fractional_widths = [fractional_widths[segment_index]]
        current_points.extend(
            [
                localize(control_1, page_index),
                localize(control_2, page_index),
                localize(end, page_index),
            ]
        )
        current_pressures.append(pressures[segment_index + 1])
        current_fractional_widths.append(fractional_widths[segment_index + 1])
    flush()
    return curves


def parse_curves(
    archive: KeyedArchive,
    rich_text: dict[str, Any],
    page_height_doc: float,
) -> tuple[StrokeCurve, ...]:
    overlay = archive.deref(rich_text.get("Handwriting Overlay"))
    if not isinstance(overlay, dict):
        return ()
    spatial_hash = archive.deref(overlay.get("SpatialHash"))
    if not isinstance(spatial_hash, dict):
        return ()
    counts = unpack_numeric_blob(spatial_hash.get("curvesnumpoints", b""), "i")
    widths = unpack_numeric_blob(spatial_hash.get("curveswidth", b""), "f")
    points_blob = unpack_numeric_blob(spatial_hash.get("curvespoints", b""), "f")
    pressures_blob = unpack_numeric_blob(spatial_hash.get("curvesforces", b""), "f")
    fractional_widths_blob = unpack_numeric_blob(spatial_hash.get("curvesfractionalwidths", b""), "f")
    styles_blob = spatial_hash.get("curvesstyles", b"")
    colors_blob = spatial_hash.get("curvescolors", b"")
    sample_counts = tuple(bezier_sample_count(int(count)) for count in counts)
    use_bezier_samples = (
        len(pressures_blob) == sum(sample_counts)
        and len(fractional_widths_blob) == sum(sample_counts)
    )
    pressure_curve_indices = []
    if not use_bezier_samples:
        pressure_curve_indices = [
            index
            for index, _ in enumerate(counts)
            if (styles_blob[index] if index < len(styles_blob) else STROKE_STYLE_PEN) == STROKE_STYLE_PENCIL
        ]
    remaining_pressure_samples = len(pressures_blob)
    remaining_fractional_samples = len(fractional_widths_blob)
    remaining_pressure_points = sum(int(counts[index]) for index in pressure_curve_indices)
    pressure_index = 0
    fractional_width_index = 0
    points_index = 0
    curves: list[StrokeCurve] = []
    for curve_index, point_count in enumerate(counts):
        points: list[tuple[float, float]] = []
        for _ in range(point_count):
            x = float(points_blob[points_index])
            y = float(points_blob[points_index + 1])
            points.append((x, y))
            points_index += 2
        rgba = parse_handwriting_color(colors_blob[curve_index * 4 : curve_index * 4 + 4])
        width = float(widths[curve_index]) if curve_index < len(widths) else 1.0
        style = styles_blob[curve_index] if curve_index < len(styles_blob) else STROKE_STYLE_PEN
        if use_bezier_samples:
            sample_count = sample_counts[curve_index]
            raw_pressures = tuple(pressures_blob[pressure_index : pressure_index + sample_count])
            raw_fractional_widths = tuple(
                fractional_widths_blob[fractional_width_index : fractional_width_index + sample_count]
            )
            pressure_index += sample_count
            fractional_width_index += sample_count
            pressures = raw_pressures or tuple(1.0 for _ in range(sample_count))
            fractional_widths = raw_fractional_widths or tuple(1.0 for _ in range(sample_count))
        elif style == STROKE_STYLE_PENCIL and remaining_pressure_points > 0:
            if curve_index == pressure_curve_indices[-1]:
                pressure_sample_count = remaining_pressure_samples
                fractional_sample_count = remaining_fractional_samples
            else:
                pressure_sample_count = round(remaining_pressure_samples * point_count / remaining_pressure_points)
                fractional_sample_count = round(remaining_fractional_samples * point_count / remaining_pressure_points)
            pressure_sample_count = max(0, min(remaining_pressure_samples, pressure_sample_count))
            fractional_sample_count = max(0, min(remaining_fractional_samples, fractional_sample_count))
            raw_pressures = tuple(pressures_blob[pressure_index : pressure_index + pressure_sample_count])
            raw_fractional_widths = tuple(
                fractional_widths_blob[
                    fractional_width_index : fractional_width_index + fractional_sample_count
                ]
            )
            pressure_index += pressure_sample_count
            fractional_width_index += fractional_sample_count
            remaining_pressure_samples -= pressure_sample_count
            remaining_fractional_samples -= fractional_sample_count
            remaining_pressure_points -= point_count
            pressures = resample_values(raw_pressures, point_count)
            fractional_widths = resample_values(raw_fractional_widths, point_count)
        else:
            pressures = tuple(1.0 for _ in range(point_count))
            fractional_widths = tuple(1.0 for _ in range(point_count))
        curves.extend(
            split_curve_into_pages(
                points,
                width,
                rgba,
                style,
                pressures,
                fractional_widths,
                page_height_doc,
            )
        )
    return tuple(curves)


def load_note_document(note_path: Path) -> NoteDocument:
    with zipfile.ZipFile(note_path) as bundle:
        entries = bundle.namelist()
        bundle_root = find_bundle_root(entries)
        session_bytes = bundle.read(bundle_root + "Session.plist")
        archive = KeyedArchive(plistlib.loads(session_bytes))
        session = archive.root
        if not isinstance(session, dict):
            raise ValueError("Unexpected Session.plist root object")

        rich_text = archive.deref(session["richText"])
        if not isinstance(rich_text, dict):
            raise ValueError("Unexpected richText object")
        reflow_state = archive.deref(rich_text["reflowState"])
        page_width_doc = float(reflow_state["pageWidthInDocumentCoordsKey"])

        paper_model = archive.deref(session.get("NBNoteTakingSessionDocumentPaperLayoutModelKey"))
        paper_attributes = (
            archive.deref(paper_model.get("documentPaperAttributes"))
            if isinstance(paper_model, dict)
            else {}
        )
        line_style = None
        paper_size = None
        if isinstance(paper_attributes, dict):
            if "lineStyle2" in paper_attributes:
                line_style = archive.as_text(paper_attributes["lineStyle2"])
            if "paperSize" in paper_attributes:
                paper_size = archive.as_text(paper_attributes["paperSize"])
            paper_sizing_behavior = (
                archive.as_text(paper_attributes["paperSizingBehavior"])
                if "paperSizingBehavior" in paper_attributes
                else None
            )
        else:
            paper_sizing_behavior = None

        export_width_pt = choose_export_width(paper_size)
        pdf_pages = parse_pdf_pages(archive, rich_text)
        thumb_entry = choose_thumbnail_entry(entries, bundle_root)
        if paper_sizing_behavior == "staticWidth" and paper_size and paper_size.lower() == "letter":
            page_ratio = math.floor(page_width_doc * 11.0 / 8.5) / page_width_doc
        elif pdf_pages:
            first_pdf = pdf_pages[0]
            with bundle.open(bundle_root + first_pdf.relative_path) as pdf_file:
                pdf_reader = PdfReader(BytesIO(pdf_file.read()), strict=False)
                source_page = pdf_reader.pages[first_pdf.source_page_index]
                page_ratio = float(source_page.mediabox.height) / float(source_page.mediabox.width)
        elif thumb_entry:
            with Image.open(bundle.open(thumb_entry)) as thumb:
                page_ratio = thumb.height / thumb.width
        else:
            page_ratio = DEFAULT_PAGE_RATIO
        export_height_pt = export_width_pt * page_ratio
        page_height_doc = page_width_doc * page_ratio

        text, text_spans = parse_text(archive, rich_text)
        media_images = parse_media_images(archive, rich_text, page_height_doc)
        curves = parse_curves(archive, rich_text, page_height_doc)

    return NoteDocument(
        bundle_root=bundle_root,
        page_width_doc=page_width_doc,
        page_height_doc=page_height_doc,
        export_width_pt=export_width_pt,
        export_height_pt=export_height_pt,
        line_style=line_style,
        text=text,
        text_spans=text_spans,
        media_images=media_images,
        pdf_pages=pdf_pages,
        curves=curves,
    )
