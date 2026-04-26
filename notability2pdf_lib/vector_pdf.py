from __future__ import annotations

import base64
import logging
import tempfile
import zipfile
from collections.abc import Callable
from io import BytesIO
from pathlib import Path
from xml.sax.saxutils import escape

import cairo
from PIL import Image
from pypdf import PdfReader, PdfWriter, Transformation
from pypdf._page import PageObject

from .constants import (
    DEFAULT_PAGE_COLOR,
    DEFAULT_RULE_COLOR,
    DEFAULT_TEXT_TOP_DOC,
    STROKE_STYLE_HIGHLIGHTER,
    STROKE_STYLE_NOT_EXPORTED,
    STROKE_STYLE_PENCIL,
)
from .fonts import measure_text_px
from .models import EmbeddedPdfPage, MediaImage, NoteDocument, StrokeCurve
from .parser import parse_content_inset_doc, parse_line_spacing_doc, resample_values
from .rendering import build_style_map, clamp


logging.getLogger("pypdf").setLevel(logging.ERROR)

PEN_WIDTH_MULTIPLIER = 1.0
CURVE_SMOOTHING_TENSION = 0.8


def rgb_float(rgba: tuple[int, int, int, int]) -> tuple[float, float, float, float]:
    return rgba[0] / 255.0, rgba[1] / 255.0, rgba[2] / 255.0, rgba[3] / 255.0


def rgb_hex(rgba: tuple[int, int, int, int]) -> str:
    return f"#{rgba[0]:02x}{rgba[1]:02x}{rgba[2]:02x}"


def page_count_for(note: NoteDocument) -> int:
    page_count = 1
    if note.pdf_pages:
        page_count = max(page_count, max(page.page_index for page in note.pdf_pages) + 1)
    if note.media_images:
        page_count = max(page_count, max(media.page_index for media in note.media_images) + 1)
    if note.curves:
        page_count = max(page_count, max(curve.page_index for curve in note.curves) + 1)
    return page_count


def pdf_page_map(note: NoteDocument) -> dict[int, EmbeddedPdfPage]:
    return {page.page_index: page for page in note.pdf_pages}


def stroke_points_pt(
    curve: StrokeCurve,
    doc_to_pt: float,
    content_inset_doc: float,
) -> list[tuple[float, float]]:
    return [((x + content_inset_doc) * doc_to_pt, y * doc_to_pt) for x, y in curve.points]


def is_bezier_point_sequence(points: list[tuple[float, float]]) -> bool:
    return len(points) >= 4 and (len(points) - 1) % 3 == 0


def bezier_segment_count(points: list[tuple[float, float]]) -> int:
    if not is_bezier_point_sequence(points):
        return 0
    return (len(points) - 1) // 3


def bezier_segments(
    points: list[tuple[float, float]],
) -> list[tuple[tuple[float, float], tuple[float, float], tuple[float, float], tuple[float, float]]]:
    if not is_bezier_point_sequence(points):
        return []
    return [
        (points[index], points[index + 1], points[index + 2], points[index + 3])
        for index in range(0, len(points) - 1, 3)
    ]


def curve_path(points: list[tuple[float, float]]) -> str:
    if not points:
        return ""
    parts = [f"M {points[0][0]:.4f} {points[0][1]:.4f}"]
    if is_bezier_point_sequence(points):
        for _, control_1, control_2, end in bezier_segments(points):
            parts.append(
                f"C {control_1[0]:.4f} {control_1[1]:.4f} "
                f"{control_2[0]:.4f} {control_2[1]:.4f} "
                f"{end[0]:.4f} {end[1]:.4f}"
            )
        return " ".join(parts)
    if len(points) == 2:
        parts.append(f"L {points[1][0]:.4f} {points[1][1]:.4f}")
        return " ".join(parts)
    for control_1, control_2, end in smoothed_cubic_segments(points):
        parts.append(
            f"C {control_1[0]:.4f} {control_1[1]:.4f} "
            f"{control_2[0]:.4f} {control_2[1]:.4f} "
            f"{end[0]:.4f} {end[1]:.4f}"
        )
    return " ".join(parts)


def smoothed_cubic_segments(
    points: list[tuple[float, float]],
) -> list[tuple[tuple[float, float], tuple[float, float], tuple[float, float]]]:
    if len(points) < 3:
        return []

    segments: list[tuple[tuple[float, float], tuple[float, float], tuple[float, float]]] = []
    scale = CURVE_SMOOTHING_TENSION / 6.0
    for index in range(len(points) - 1):
        p0 = points[index - 1] if index > 0 else points[index]
        p1 = points[index]
        p2 = points[index + 1]
        p3 = points[index + 2] if index + 2 < len(points) else p2
        control_1 = (p1[0] + (p2[0] - p0[0]) * scale, p1[1] + (p2[1] - p0[1]) * scale)
        control_2 = (p2[0] - (p3[0] - p1[0]) * scale, p2[1] - (p3[1] - p1[1]) * scale)
        segments.append((control_1, control_2, p2))
    return segments


def pen_width_pt(curve: StrokeCurve, doc_to_pt: float) -> float:
    multiplier = 1.0 if curve.style == STROKE_STYLE_HIGHLIGHTER else PEN_WIDTH_MULTIPLIER
    return max(0.1, curve.width * doc_to_pt * multiplier)


def pencil_segment_width_pt(curve: StrokeCurve, doc_to_pt: float, pressure: float, fractional_width: float) -> float:
    base_width = max(0.1, curve.width * doc_to_pt)
    pressure = clamp(pressure, 0.05, 2.5)
    fractional_width = clamp(fractional_width, 0.15, 3.0)
    return max(0.1, base_width * fractional_width * (0.45 + 0.38 * pressure**0.5))


def pencil_segment_alpha(alpha: float, pressure: float) -> float:
    return alpha * clamp(0.18 + 0.34 * pressure, 0.14, 0.96)


def draw_curve_path_cairo(ctx: cairo.Context, points: list[tuple[float, float]]) -> None:
    ctx.move_to(*points[0])
    if is_bezier_point_sequence(points):
        for _, control_1, control_2, end in bezier_segments(points):
            ctx.curve_to(control_1[0], control_1[1], control_2[0], control_2[1], end[0], end[1])
        return
    if len(points) == 2:
        ctx.line_to(*points[1])
        return
    for control_1, control_2, end in smoothed_cubic_segments(points):
        ctx.curve_to(control_1[0], control_1[1], control_2[0], control_2[1], end[0], end[1])


def draw_background_cairo(
    ctx: cairo.Context,
    note: NoteDocument,
    doc_to_pt: float,
    content_inset_doc: float,
    line_spacing_doc: float,
) -> None:
    red, green, blue, alpha = rgb_float(DEFAULT_PAGE_COLOR)
    ctx.set_source_rgba(red, green, blue, alpha)
    ctx.rectangle(0, 0, note.export_width_pt, note.export_height_pt)
    ctx.fill()

    if not note.line_style or not note.line_style.startswith("Lines"):
        return

    red, green, blue, alpha = rgb_float(DEFAULT_RULE_COLOR)
    ctx.set_source_rgba(red, green, blue, alpha)
    ctx.set_line_width(0.5)
    margin = content_inset_doc * doc_to_pt
    y = line_spacing_doc * doc_to_pt
    while y < note.export_height_pt:
        ctx.move_to(margin, y)
        ctx.line_to(note.export_width_pt - margin, y)
        y += line_spacing_doc * doc_to_pt
    ctx.stroke()


def background_svg(
    note: NoteDocument,
    doc_to_pt: float,
    content_inset_doc: float,
    line_spacing_doc: float,
) -> list[str]:
    elements = [
        f'<rect x="0" y="0" width="{note.export_width_pt:.4f}" '
        f'height="{note.export_height_pt:.4f}" fill="{rgb_hex(DEFAULT_PAGE_COLOR)}"/>'
    ]
    if not note.line_style or not note.line_style.startswith("Lines"):
        return elements

    margin = content_inset_doc * doc_to_pt
    y = line_spacing_doc * doc_to_pt
    while y < note.export_height_pt:
        elements.append(
            f'<line x1="{margin:.4f}" y1="{y:.4f}" '
            f'x2="{note.export_width_pt - margin:.4f}" y2="{y:.4f}" '
            f'stroke="{rgb_hex(DEFAULT_RULE_COLOR)}" stroke-width="0.5"/>'
        )
        y += line_spacing_doc * doc_to_pt
    return elements


def draw_text_cairo(
    ctx: cairo.Context,
    note: NoteDocument,
    doc_to_pt: float,
    content_inset_doc: float,
    line_spacing_doc: float,
) -> None:
    style_map = build_style_map(len(note.text), note.text_spans)
    cursor_x_doc = 0.0
    cursor_y_doc = DEFAULT_TEXT_TOP_DOC

    for index, char in enumerate(note.text):
        if char == "\r":
            continue
        if char == "\n":
            cursor_x_doc = 0.0
            cursor_y_doc += line_spacing_doc
            continue

        style = style_map[index]
        font_pt = style.font_size * doc_to_pt
        char_width_doc = measure_text_px(char, style.font_name, max(1, round(font_pt))) / doc_to_pt
        if content_inset_doc + cursor_x_doc + char_width_doc > note.page_width_doc - content_inset_doc:
            cursor_x_doc = 0.0
            cursor_y_doc += line_spacing_doc

        page_index = int(cursor_y_doc // note.page_height_doc)
        if page_index != 0:
            cursor_x_doc += char_width_doc
            continue

        red, green, blue, alpha = rgb_float(style.color)
        ctx.set_source_rgba(red, green, blue, alpha)
        ctx.select_font_face("Helvetica", cairo.FONT_SLANT_NORMAL, cairo.FONT_WEIGHT_NORMAL)
        ctx.set_font_size(font_pt)
        _, _, _, _, _, y_advance = ctx.text_extents(char)
        x = (content_inset_doc + cursor_x_doc) * doc_to_pt
        y = (cursor_y_doc * doc_to_pt) - y_advance + font_pt
        ctx.move_to(x, y)
        ctx.show_text(char)
        cursor_x_doc += char_width_doc


def text_svg_for_page(
    note: NoteDocument,
    page_index_filter: int,
    doc_to_pt: float,
    content_inset_doc: float,
    line_spacing_doc: float,
) -> list[str]:
    style_map = build_style_map(len(note.text), note.text_spans)
    cursor_x_doc = 0.0
    cursor_y_doc = DEFAULT_TEXT_TOP_DOC
    elements: list[str] = []

    for index, char in enumerate(note.text):
        if char == "\r":
            continue
        if char == "\n":
            cursor_x_doc = 0.0
            cursor_y_doc += line_spacing_doc
            continue

        style = style_map[index]
        font_pt = style.font_size * doc_to_pt
        char_width_doc = measure_text_px(char, style.font_name, max(1, round(font_pt))) / doc_to_pt
        if content_inset_doc + cursor_x_doc + char_width_doc > note.page_width_doc - content_inset_doc:
            cursor_x_doc = 0.0
            cursor_y_doc += line_spacing_doc

        page_index = int(cursor_y_doc // note.page_height_doc)
        if page_index == page_index_filter:
            local_y_doc = cursor_y_doc - page_index * note.page_height_doc
            x = (content_inset_doc + cursor_x_doc) * doc_to_pt
            y = local_y_doc * doc_to_pt + font_pt
            opacity = style.color[3] / 255.0
            elements.append(
                f'<text x="{x:.4f}" y="{y:.4f}" font-family="Helvetica" '
                f'font-size="{font_pt:.4f}" fill="{rgb_hex(style.color)}" '
                f'fill-opacity="{opacity:.4f}">{escape(char)}</text>'
            )
        cursor_x_doc += char_width_doc
    return elements


def draw_image_cairo(
    ctx: cairo.Context,
    temp_dir: Path,
    bundle: zipfile.ZipFile,
    note: NoteDocument,
    media: MediaImage,
    doc_to_pt: float,
    content_inset_doc: float,
) -> None:
    with Image.open(bundle.open(note.bundle_root + media.relative_path)) as image:
        png_path = temp_dir / f"image-{abs(hash((media.relative_path, media.page_index, media.x, media.y)))}.png"
        image.convert("RGBA").save(png_path)
    surface = cairo.ImageSurface.create_from_png(str(png_path))
    x = (content_inset_doc + media.x) * doc_to_pt
    y = media.y * doc_to_pt
    width = media.width * doc_to_pt
    height = media.height * doc_to_pt
    ctx.save()
    ctx.translate(x, y)
    ctx.scale(width / surface.get_width(), height / surface.get_height())
    ctx.set_source_surface(surface, 0, 0)
    ctx.paint()
    ctx.restore()


def image_svg(
    bundle: zipfile.ZipFile,
    note: NoteDocument,
    media: MediaImage,
    doc_to_pt: float,
    content_inset_doc: float,
) -> str:
    data = bundle.read(note.bundle_root + media.relative_path)
    suffix = Path(media.relative_path).suffix.lower()
    mime = "image/png" if suffix == ".png" else "image/jpeg"
    encoded = base64.b64encode(data).decode("ascii")
    x = (content_inset_doc + media.x) * doc_to_pt
    y = media.y * doc_to_pt
    width = media.width * doc_to_pt
    height = media.height * doc_to_pt
    return (
        f'<image x="{x:.4f}" y="{y:.4f}" width="{width:.4f}" height="{height:.4f}" '
        f'href="data:{mime};base64,{encoded}"/>'
    )


def draw_pen_curve_cairo(ctx: cairo.Context, curve: StrokeCurve, doc_to_pt: float, content_inset_doc: float) -> None:
    points = stroke_points_pt(curve, doc_to_pt, content_inset_doc)
    if not points:
        return
    red, green, blue, alpha = rgb_float(curve.rgba)
    ctx.set_source_rgba(red, green, blue, alpha)
    ctx.set_line_width(pen_width_pt(curve, doc_to_pt))
    ctx.set_line_cap(cairo.LineCap.ROUND)
    ctx.set_line_join(cairo.LineJoin.ROUND)
    if len(points) == 1:
        radius = max(0.5, pen_width_pt(curve, doc_to_pt) / 2.0)
        ctx.arc(points[0][0], points[0][1], radius, 0, 6.283185307179586)
        ctx.fill()
    else:
        draw_curve_path_cairo(ctx, points)
        ctx.stroke()


def draw_highlighter_curve_cairo(ctx: cairo.Context, curve: StrokeCurve, doc_to_pt: float, content_inset_doc: float) -> None:
    ctx.save()
    if hasattr(cairo, "OPERATOR_MULTIPLY"):
        ctx.set_operator(cairo.OPERATOR_MULTIPLY)
    draw_pen_curve_cairo(ctx, curve, doc_to_pt, content_inset_doc)
    ctx.restore()


def draw_pencil_curve_cairo(ctx: cairo.Context, curve: StrokeCurve, doc_to_pt: float, content_inset_doc: float) -> None:
    points = stroke_points_pt(curve, doc_to_pt, content_inset_doc)
    if not points:
        return

    red, green, blue, alpha = rgb_float(curve.rgba)
    ctx.set_line_cap(cairo.LineCap.ROUND)
    ctx.set_line_join(cairo.LineJoin.ROUND)
    if is_bezier_point_sequence(points):
        sample_count = bezier_segment_count(points) + 1
        pressures = resample_values(curve.pressures, sample_count)
        fractional_widths = resample_values(curve.fractional_widths, sample_count)
        for index, (start, control_1, control_2, end) in enumerate(bezier_segments(points)):
            pressure = (pressures[index] + pressures[index + 1]) / 2.0
            fractional_width = (fractional_widths[index] + fractional_widths[index + 1]) / 2.0
            ctx.set_source_rgba(red, green, blue, pencil_segment_alpha(alpha, pressure))
            ctx.set_line_width(pencil_segment_width_pt(curve, doc_to_pt, pressure, fractional_width))
            ctx.move_to(*start)
            ctx.curve_to(control_1[0], control_1[1], control_2[0], control_2[1], end[0], end[1])
            ctx.stroke()
        return

    pressures = resample_values(curve.pressures, len(points))
    fractional_widths = resample_values(curve.fractional_widths, len(points))
    for index, (start, end) in enumerate(zip(points, points[1:], strict=False)):
        pressure = clamp((pressures[index] + pressures[index + 1]) / 2.0, 0.05, 2.5)
        fractional_width = clamp((fractional_widths[index] + fractional_widths[index + 1]) / 2.0, 0.15, 3.0)
        ctx.set_source_rgba(red, green, blue, pencil_segment_alpha(alpha, pressure))
        ctx.set_line_width(pencil_segment_width_pt(curve, doc_to_pt, pressure, fractional_width))
        ctx.move_to(*start)
        ctx.line_to(*end)
        ctx.stroke()


def curve_svg(curve: StrokeCurve, doc_to_pt: float, content_inset_doc: float) -> list[str]:
    points = stroke_points_pt(curve, doc_to_pt, content_inset_doc)
    if not points:
        return []
    if curve.style == STROKE_STYLE_PENCIL and len(points) > 1:
        elements: list[str] = []
        if is_bezier_point_sequence(points):
            sample_count = bezier_segment_count(points) + 1
            pressures = resample_values(curve.pressures, sample_count)
            fractional_widths = resample_values(curve.fractional_widths, sample_count)
            for index, (start, control_1, control_2, end) in enumerate(bezier_segments(points)):
                pressure = (pressures[index] + pressures[index + 1]) / 2.0
                fractional_width = (fractional_widths[index] + fractional_widths[index + 1]) / 2.0
                opacity = pencil_segment_alpha(curve.rgba[3] / 255.0, pressure)
                width = pencil_segment_width_pt(curve, doc_to_pt, pressure, fractional_width)
                path = (
                    f"M {start[0]:.4f} {start[1]:.4f} "
                    f"C {control_1[0]:.4f} {control_1[1]:.4f} "
                    f"{control_2[0]:.4f} {control_2[1]:.4f} "
                    f"{end[0]:.4f} {end[1]:.4f}"
                )
                elements.append(
                    f'<path d="{path}" fill="none" stroke="{rgb_hex(curve.rgba)}" '
                    f'stroke-opacity="{opacity:.4f}" stroke-width="{width:.4f}" '
                    f'stroke-linecap="round" stroke-linejoin="round"/>'
                )
            return elements

        pressures = resample_values(curve.pressures, len(points))
        fractional_widths = resample_values(curve.fractional_widths, len(points))
        for index, (start, end) in enumerate(zip(points, points[1:], strict=False)):
            pressure = (pressures[index] + pressures[index + 1]) / 2.0
            fractional_width = (fractional_widths[index] + fractional_widths[index + 1]) / 2.0
            width = pencil_segment_width_pt(curve, doc_to_pt, pressure, fractional_width)
            opacity = pencil_segment_alpha(curve.rgba[3] / 255.0, pressure)
            elements.append(
                f'<line x1="{start[0]:.4f}" y1="{start[1]:.4f}" x2="{end[0]:.4f}" y2="{end[1]:.4f}" '
                f'stroke="{rgb_hex(curve.rgba)}" stroke-opacity="{opacity:.4f}" '
                f'stroke-width="{width:.4f}" stroke-linecap="round" stroke-linejoin="round"/>'
            )
        return elements

    opacity = curve.rgba[3] / 255.0
    blend = ' style="mix-blend-mode:multiply"' if curve.style == STROKE_STYLE_HIGHLIGHTER else ""
    return [
        f'<path d="{curve_path(points)}" fill="none" stroke="{rgb_hex(curve.rgba)}" '
        f'stroke-opacity="{opacity:.4f}" stroke-width="{pen_width_pt(curve, doc_to_pt):.4f}" '
        f'stroke-linecap="round" stroke-linejoin="round"{blend}/>'
    ]


def draw_curves_cairo(ctx: cairo.Context, curves: list[StrokeCurve], doc_to_pt: float, content_inset_doc: float) -> None:
    highlighter_curves = [curve for curve in curves if curve.style == STROKE_STYLE_HIGHLIGHTER]
    pen_curves = [
        curve
        for curve in curves
        if curve.style not in {STROKE_STYLE_HIGHLIGHTER, STROKE_STYLE_PENCIL, STROKE_STYLE_NOT_EXPORTED}
    ]
    pencil_curves = [curve for curve in curves if curve.style == STROKE_STYLE_PENCIL]
    for curve in highlighter_curves:
        draw_highlighter_curve_cairo(ctx, curve, doc_to_pt, content_inset_doc)
    for curve in pen_curves:
        draw_pen_curve_cairo(ctx, curve, doc_to_pt, content_inset_doc)
    for curve in pencil_curves:
        draw_pencil_curve_cairo(ctx, curve, doc_to_pt, content_inset_doc)


def write_svg_pages(note: NoteDocument, note_path: Path, svg_dir: Path) -> None:
    svg_dir.mkdir(parents=True, exist_ok=True)
    doc_to_pt = note.export_width_pt / note.page_width_doc
    line_spacing_doc = parse_line_spacing_doc(note.line_style, note.page_width_doc, note.export_width_pt)
    content_inset_doc = parse_content_inset_doc(note.page_width_doc)
    pdf_pages = pdf_page_map(note)
    page_count = page_count_for(note)
    media_by_page: dict[int, list[MediaImage]] = {}
    curves_by_page: dict[int, list[StrokeCurve]] = {}
    for media in note.media_images:
        media_by_page.setdefault(media.page_index, []).append(media)
    for curve in note.curves:
        curves_by_page.setdefault(curve.page_index, []).append(curve)

    with zipfile.ZipFile(note_path) as bundle:
        for page_index in range(page_count):
            elements = [
                f'<svg xmlns="http://www.w3.org/2000/svg" width="{note.export_width_pt:.4f}pt" '
                f'height="{note.export_height_pt:.4f}pt" viewBox="0 0 {note.export_width_pt:.4f} {note.export_height_pt:.4f}">'
            ]
            if page_index in pdf_pages:
                elements.append(
                    f"<!-- Base PDF page: {escape(pdf_pages[page_index].relative_path)} "
                    f"page {pdf_pages[page_index].source_page_index + 1}; SVG contains overlays only. -->"
                )
            else:
                elements.extend(background_svg(note, doc_to_pt, content_inset_doc, line_spacing_doc))
            elements.extend(text_svg_for_page(note, page_index, doc_to_pt, content_inset_doc, line_spacing_doc))
            for media in media_by_page.get(page_index, []):
                elements.append(image_svg(bundle, note, media, doc_to_pt, content_inset_doc))
            for curve in curves_by_page.get(page_index, []):
                elements.extend(curve_svg(curve, doc_to_pt, content_inset_doc))
            elements.append("</svg>")
            (svg_dir / f"page-{page_index + 1}.svg").write_text("\n".join(elements), encoding="utf-8")


def write_cairo_pdf(note: NoteDocument, note_path: Path, output_path: Path, draw_pdf_background_pages: bool) -> None:
    output_path.parent.mkdir(parents=True, exist_ok=True)
    doc_to_pt = note.export_width_pt / note.page_width_doc
    line_spacing_doc = parse_line_spacing_doc(note.line_style, note.page_width_doc, note.export_width_pt)
    content_inset_doc = parse_content_inset_doc(note.page_width_doc)
    pdf_pages = pdf_page_map(note)
    page_count = page_count_for(note)
    media_by_page: dict[int, list[MediaImage]] = {}
    curves_by_page: dict[int, list[StrokeCurve]] = {}
    for media in note.media_images:
        media_by_page.setdefault(media.page_index, []).append(media)
    for curve in note.curves:
        curves_by_page.setdefault(curve.page_index, []).append(curve)

    with tempfile.TemporaryDirectory() as temp_name, zipfile.ZipFile(note_path) as bundle:
        temp_dir = Path(temp_name)
        surface = cairo.PDFSurface(str(output_path), note.export_width_pt, note.export_height_pt)
        ctx = cairo.Context(surface)
        for page_index in range(page_count):
            surface.set_size(note.export_width_pt, note.export_height_pt)
            if draw_pdf_background_pages or page_index not in pdf_pages:
                draw_background_cairo(ctx, note, doc_to_pt, content_inset_doc, line_spacing_doc)
            if page_index == 0:
                draw_text_cairo(ctx, note, doc_to_pt, content_inset_doc, line_spacing_doc)
            for media in media_by_page.get(page_index, []):
                draw_image_cairo(ctx, temp_dir, bundle, note, media, doc_to_pt, content_inset_doc)
            draw_curves_cairo(ctx, curves_by_page.get(page_index, []), doc_to_pt, content_inset_doc)
            ctx.show_page()
        surface.finish()


def merge_embedded_pdf_pages(note: NoteDocument, note_path: Path, overlay_path: Path, output_path: Path) -> None:
    output_path.parent.mkdir(parents=True, exist_ok=True)
    overlay_reader = PdfReader(str(overlay_path), strict=False)
    writer = PdfWriter()
    page_count = page_count_for(note)
    pdf_pages = pdf_page_map(note)

    with zipfile.ZipFile(note_path) as bundle:
        source_readers: dict[str, PdfReader] = {}
        for page_index in range(page_count):
            base = PageObject.create_blank_page(width=note.export_width_pt, height=note.export_height_pt)
            pdf_page = pdf_pages.get(page_index)
            if pdf_page:
                reader = source_readers.get(pdf_page.relative_path)
                if reader is None:
                    data = bundle.read(note.bundle_root + pdf_page.relative_path)
                    reader = PdfReader(BytesIO(data), strict=False)
                    source_readers[pdf_page.relative_path] = reader
                source = reader.pages[pdf_page.source_page_index]
                sx = note.export_width_pt / float(source.mediabox.width)
                sy = note.export_height_pt / float(source.mediabox.height)
                base.merge_transformed_page(source, Transformation().scale(sx, sy))
            if page_index < len(overlay_reader.pages):
                base.merge_page(overlay_reader.pages[page_index])
            writer.add_page(base)
    with output_path.open("wb") as output_file:
        writer.write(output_file)


def write_vector_pdf(note: NoteDocument, note_path: Path, output_path: Path, svg_dir: Path | None = None) -> None:
    if svg_dir is not None:
        write_svg_pages(note, note_path, svg_dir)

    if not note.pdf_pages:
        write_cairo_pdf(note, note_path, output_path, draw_pdf_background_pages=True)
        return

    with tempfile.TemporaryDirectory() as temp_name:
        overlay_path = Path(temp_name) / "overlay.pdf"
        write_cairo_pdf(note, note_path, overlay_path, draw_pdf_background_pages=False)
        merge_embedded_pdf_pages(note, note_path, overlay_path, output_path)
