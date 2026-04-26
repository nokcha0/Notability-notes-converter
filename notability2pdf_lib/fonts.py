from __future__ import annotations

from functools import lru_cache
from pathlib import Path

from PIL import ImageFont

from .constants import FONT_FALLBACKS


def resolve_font_path(font_name: str) -> str:
    normalized = font_name.lower().replace(" ", "")
    for key, candidates in FONT_FALLBACKS.items():
        if key in normalized:
            for candidate in candidates:
                if Path(candidate).exists():
                    return candidate
    for candidate in FONT_FALLBACKS["arial"]:
        if Path(candidate).exists():
            return candidate
    return ""


@lru_cache(maxsize=256)
def load_font(font_name: str, size_px: int) -> ImageFont.ImageFont:
    size_px = max(1, size_px)
    font_path = resolve_font_path(font_name)
    if font_path:
        try:
            return ImageFont.truetype(font_path, size_px)
        except OSError:
            pass
    return ImageFont.load_default()


@lru_cache(maxsize=4096)
def measure_text_px(text: str, font_name: str, size_px: int) -> float:
    font = load_font(font_name, size_px)
    if hasattr(font, "getlength"):
        return float(font.getlength(text))
    bbox = font.getbbox(text)
    return float(bbox[2] - bbox[0])
