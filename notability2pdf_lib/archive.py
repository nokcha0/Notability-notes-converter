from __future__ import annotations

import plistlib
from typing import Any


UID = plistlib.UID


class KeyedArchive:
    def __init__(self, payload: dict[str, Any]):
        self.objects = payload["$objects"]
        top = payload["$top"]
        self.root = self.deref(next(iter(top.values())))

    def deref(self, value: Any) -> Any:
        if isinstance(value, UID):
            return self.objects[value.data]
        return value

    def ns_array(self, value: Any) -> list[Any]:
        obj = self.deref(value)
        if not isinstance(obj, dict) or "NS.objects" not in obj:
            return []
        return [self.deref(item) for item in obj["NS.objects"]]

    def ns_dict(self, value: Any) -> dict[str, Any]:
        obj = self.deref(value)
        if not isinstance(obj, dict):
            return {}
        if "NS.keys" not in obj or "NS.objects" not in obj:
            return obj
        keys = [self.as_text(item) for item in obj["NS.keys"]]
        values = [self.deref(item) for item in obj["NS.objects"]]
        return dict(zip(keys, values, strict=False))

    def as_text(self, value: Any) -> str:
        obj = self.deref(value)
        if isinstance(obj, str):
            return obj
        if isinstance(obj, bytes):
            return obj.decode("utf-8")
        if isinstance(obj, dict) and "NS.bytes" in obj:
            return obj["NS.bytes"].decode("utf-8")
        raise TypeError(f"Unsupported text payload: {type(obj)!r}")
