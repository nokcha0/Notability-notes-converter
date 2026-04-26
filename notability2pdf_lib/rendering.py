from __future__ import annotations

import math
import zipfile
from collections.abc import Callable
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

from .constants import (
    DEFAULT_PAGE_COLOR,
    DEFAULT_RULE_COLOR,
    DEFAULT_TEXT_TOP_DOC,
    STROKE_STYLE_HIGHLIGHTER,
    STROKE_STYLE_NOT_EXPORTED,
    STROKE_STYLE_PENCIL,
)
from .fonts import load_font, measure_text_px
from .models import NoteDocument, StrokeCurve, TextSpan, TextStyle
from .parser import (
    default_text_style,
    parse_content_inset_doc,
    parse_line_spacing_doc,
    resample_values,
)


def build_style_map(text_length: int, spans: tuple[TextSpan, ...]) -> list[TextStyle]:
    styles = [default_text_style() for _ in range(text_length)]
    for span in spans:
        start = max(0, span.start)
        end = min(text_length, span.start + span.length)
        for index in range(start, end):
            styles[index] = span.style
    return styles


def draw_page_background(
    page: Image.Image,
    note: NoteDocument,
    doc_to_px: float,
    content_inset_doc: float,
    line_spacing_doc: float,
    render_scale: float,
) -> None:
    if not note.line_style or not note.line_style.startswith("Lines"):
        return

    draw = ImageDraw.Draw(page)
    margin_px = round(content_inset_doc * doc_to_px)
    line_spacing_px = max(1, round(line_spacing_doc * doc_to_px))
    line_width_px = max(1, round(0.5 * render_scale))
    y = line_spacing_px
    while y < page.height:
        draw.line((margin_px, y, page.width - margin_px, y), fill=DEFAULT_RULE_COLOR, width=line_width_px)
        y += line_spacing_px


def clamp(value: float, lower: float, upper: float) -> float:
    return max(lower, min(upper, value))


def round_half_up(value: float) -> int:
    return int(math.floor(value + 0.5))


def draw_round_polyline(
    image: Image.Image,
    points: list[tuple[float, float]],
    fill: tuple[int, int, int, int],
    width: int,
) -> None:
    if not points:
        return

    draw = ImageDraw.Draw(image, "RGBA")
    radius = max(0.5, width / 2.0)
    if len(points) == 1:
        x, y = points[0]
        draw.ellipse((x - radius, y - radius, x + radius, y + radius), fill=fill)
        return

    draw.line(points, fill=fill, width=max(1, width), joint="curve")
    for x, y in (points[0], points[-1]):
        draw.ellipse((x - radius, y - radius, x + radius, y + radius), fill=fill)


def darken_alpha_composite(base: Image.Image, overlay: Image.Image) -> None:
    alpha = overlay.getchannel("A")
    if alpha.getextrema() == (0, 0):
        return

    darkened_rgb = ImageChops.darker(base.convert("RGB"), overlay.convert("RGB")).convert("RGBA")
    blended = Image.composite(darkened_rgb, base, alpha)
    base.alpha_composite(blended)


def stroke_points_px(
    curve: StrokeCurve,
    doc_to_px: float,
    content_inset_doc: float,
) -> list[tuple[float, float]]:
    return [((x + content_inset_doc) * doc_to_px, y * doc_to_px) for x, y in curve.points]


def draw_highlighter_curve(
    page: Image.Image,
    curve: StrokeCurve,
    doc_to_px: float,
    content_inset_doc: float,
) -> None:
    points = stroke_points_px(curve, doc_to_px, content_inset_doc)
    width = max(1, round(curve.width * doc_to_px))
    overlay = Image.new("RGBA", page.size, (0, 0, 0, 0))
    draw_round_polyline(overlay, points, curve.rgba, width)
    darken_alpha_composite(page, overlay)


def draw_pen_curve(
    page: Image.Image,
    curve: StrokeCurve,
    doc_to_px: float,
    content_inset_doc: float,
) -> None:
    points = stroke_points_px(curve, doc_to_px, content_inset_doc)
    width = max(1, round(curve.width * doc_to_px))
    draw_round_polyline(page, points, curve.rgba, width)


def draw_pencil_curve(
    page: Image.Image,
    curve: StrokeCurve,
    doc_to_px: float,
    content_inset_doc: float,
) -> None:
    points = stroke_points_px(curve, doc_to_px, content_inset_doc)
    if not points:
        return

    base_width = max(1.0, curve.width * doc_to_px)
    pressures = resample_values(curve.pressures, len(points))
    fractional_widths = resample_values(curve.fractional_widths, len(points))
    red, green, blue, alpha = curve.rgba

    if len(points) == 1:
        overlay = Image.new("RGBA", page.size, (0, 0, 0, 0))
        draw = ImageDraw.Draw(overlay, "RGBA")
        pressure = clamp(pressures[0], 0.05, 2.5)
        radius = base_width * (0.35 + 0.35 * math.sqrt(pressure))
        fill_alpha = round(alpha * clamp(0.12 + 0.28 * pressure, 0.12, 0.92))
        x, y = points[0]
        draw.ellipse((x - radius, y - radius, x + radius, y + radius), fill=(red, green, blue, fill_alpha))
        page.alpha_composite(overlay)
        return

    alpha_mask = Image.new("L", page.size, 0)
    for index, (start, end) in enumerate(zip(points, points[1:], strict=False)):
        pressure = clamp((pressures[index] + pressures[index + 1]) / 2.0, 0.05, 2.5)
        fractional_width = clamp(
            (fractional_widths[index] + fractional_widths[index + 1]) / 2.0,
            0.15,
            3.0,
        )
        width = max(1, round(base_width * fractional_width * (0.35 + 0.35 * math.sqrt(pressure))))
        fill_alpha = round(alpha * clamp(0.12 + 0.28 * pressure, 0.12, 0.92))
        segment_mask = Image.new("L", page.size, 0)
        segment_draw = ImageDraw.Draw(segment_mask)
        segment_draw.line((start, end), fill=fill_alpha, width=width)
        alpha_mask = ImageChops.screen(alpha_mask, segment_mask)

    overlay = Image.new("RGBA", page.size, (red, green, blue, 0))
    overlay.putalpha(alpha_mask)
    page.alpha_composite(overlay)


def render_text(
    note: NoteDocument,
    doc_to_px: float,
    content_inset_doc: float,
    line_spacing_doc: float,
    ensure_page: Callable[[int], Image.Image],
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
        font_px = max(1, round(style.font_size * doc_to_px))
        char_width_doc = measure_text_px(char, style.font_name, font_px) / doc_to_px
        if content_inset_doc + cursor_x_doc + char_width_doc > note.page_width_doc - content_inset_doc:
            cursor_x_doc = 0.0
            cursor_y_doc += line_spacing_doc

        page_index = int(cursor_y_doc // note.page_height_doc)
        local_y_doc = cursor_y_doc - page_index * note.page_height_doc
        page = ensure_page(page_index)
        draw = ImageDraw.Draw(page)
        draw.text(
            ((content_inset_doc + cursor_x_doc) * doc_to_px, local_y_doc * doc_to_px),
            char,
            font=load_font(style.font_name, font_px),
            fill=style.color,
        )
        cursor_x_doc += char_width_doc


def render_media(
    note: NoteDocument,
    note_path: Path,
    doc_to_px: float,
    content_inset_doc: float,
    ensure_page: Callable[[int], Image.Image],
) -> None:
    with zipfile.ZipFile(note_path) as bundle:
        for media in note.media_images:
            page = ensure_page(media.page_index)
            try:
                with Image.open(bundle.open(note.bundle_root + media.relative_path)) as media_image:
                    rendered = media_image.convert("RGBA").resize(
                        (max(1, round(media.width * doc_to_px)), max(1, round(media.height * doc_to_px))),
                        Image.Resampling.LANCZOS,
                    )
            except KeyError:
                continue
            page.alpha_composite(
                rendered,
                (round((content_inset_doc + media.x) * doc_to_px), round(media.y * doc_to_px)),
            )


def render_curves(
    note: NoteDocument,
    doc_to_px: float,
    content_inset_doc: float,
    ensure_page: Callable[[int], Image.Image],
) -> None:
    highlighter_curves = [curve for curve in note.curves if curve.style == STROKE_STYLE_HIGHLIGHTER]
    # Style 6 is visible in Notability's thumbnail cache for the sample note, but is not emitted
    # by Notability's own PDF export. Treat it as transient/non-exported content.
    pen_curves = [
        curve
        for curve in note.curves
        if curve.style not in {STROKE_STYLE_HIGHLIGHTER, STROKE_STYLE_PENCIL, STROKE_STYLE_NOT_EXPORTED}
    ]
    pencil_curves = [curve for curve in note.curves if curve.style == STROKE_STYLE_PENCIL]

    for curve in highlighter_curves:
        draw_highlighter_curve(ensure_page(curve.page_index), curve, doc_to_px, content_inset_doc)
    for curve in pen_curves:
        draw_pen_curve(ensure_page(curve.page_index), curve, doc_to_px, content_inset_doc)
    for curve in pencil_curves:
        draw_pencil_curve(ensure_page(curve.page_index), curve, doc_to_px, content_inset_doc)


def render_note_pages(note: NoteDocument, note_path: Path, render_scale: float) -> list[Image.Image]:
    width_px = max(1, round_half_up(note.export_width_pt * render_scale))
    height_px = max(1, round_half_up(note.export_height_pt * render_scale))
    doc_to_px = width_px / note.page_width_doc
    line_spacing_doc = parse_line_spacing_doc(note.line_style, note.page_width_doc, note.export_width_pt)
    content_inset_doc = parse_content_inset_doc(note.page_width_doc)
    pages: list[Image.Image] = []

    def ensure_page(page_index: int) -> Image.Image:
        while len(pages) <= page_index:
            page = Image.new("RGBA", (width_px, height_px), DEFAULT_PAGE_COLOR)
            draw_page_background(page, note, doc_to_px, content_inset_doc, line_spacing_doc, render_scale)
            pages.append(page)
        return pages[page_index]

    render_text(note, doc_to_px, content_inset_doc, line_spacing_doc, ensure_page)
    render_media(note, note_path, doc_to_px, content_inset_doc, ensure_page)
    render_curves(note, doc_to_px, content_inset_doc, ensure_page)

    if not pages:
        pages.append(Image.new("RGBA", (width_px, height_px), DEFAULT_PAGE_COLOR))
    return [page.convert("RGB") for page in pages]
