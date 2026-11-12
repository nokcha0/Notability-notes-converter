"""Small PDF-backed note for browser background and rotation tests."""
from pathlib import Path
import plistlib
import zipfile

root = Path(__file__).resolve().parents[2] / "target" / "web-fixtures"
root.mkdir(parents=True, exist_ok=True)

stream = b"0.1 0.4 0.8 rg 20 30 60 80 re f\n"
objects = [
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] /CropBox [10 20 190 280] /Rotate 90 >>",
    b"<< /Type /Page /Parent 2 0 R /Resources << >> /Contents 4 0 R >>",
    b"<< /Length " + str(len(stream)).encode() + b" >>\nstream\n" + stream + b"endstream",
]
pdf = bytearray(b"%PDF-1.4\n")
offsets = [0]
for index, obj in enumerate(objects, 1):
    offsets.append(len(pdf))
    pdf.extend(f"{index} 0 obj\n".encode() + obj + b"\nendobj\n")
xref = len(pdf)
pdf.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
for offset in offsets[1:]:
    pdf.extend(f"{offset:010d} 00000 n \n".encode())
pdf.extend(f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode())

session = {
    "$objects": ["$null", {"richText": {
        "reflowState": {"pageWidthInDocumentCoordsKey": 679.0},
        "pageLayoutArray": {"NS.objects": [{
            "kPageLayoutDocumentPageNumberKey": number,
            "kPageLayoutPDFPageNumberKey": 1,
            "kPageLayoutPDFFileNameKey": "background.pdf",
        } for number in (1, 2)]},
    }}],
    "$top": {"root": plistlib.UID(1)},
}
with zipfile.ZipFile(root / "PDF background.note", "w", zipfile.ZIP_DEFLATED) as bundle:
    bundle.writestr("Test/Session.plist", plistlib.dumps(session, fmt=plistlib.FMT_BINARY))
    bundle.writestr("Test/PDFs/background.pdf", pdf)

long_session = plistlib.loads(plistlib.dumps(session, fmt=plistlib.FMT_BINARY))
long_session["$objects"][1]["richText"]["pageLayoutArray"]["NS.objects"] = [{
    "kPageLayoutDocumentPageNumberKey": number,
    "kPageLayoutPDFPageNumberKey": 1,
    "kPageLayoutPDFFileNameKey": "background.pdf",
} for number in range(1, 65)]
with zipfile.ZipFile(root / "Long.note", "w", zipfile.ZIP_DEFLATED) as bundle:
    bundle.writestr("Test/Session.plist", plistlib.dumps(long_session, fmt=plistlib.FMT_BINARY))
    bundle.writestr("Test/PDFs/background.pdf", pdf)

(root / "broken.ntb").write_bytes(b"not a note archive")

inputs = root.parents[1] / "input"
small_note = (root / "PDF background.note").read_bytes()
with zipfile.ZipFile(root / "bulk.zip", "w", zipfile.ZIP_DEFLATED) as bundle:
    bundle.write(inputs / "NOTE Note Apr 26, 2026.note", "Course/First.note")
    bundle.write(inputs / "NTB Note Apr 26, 2026.ntb", "Course/Second.NTB")
    bundle.writestr("Other/First.note", small_note)
    bundle.writestr("Course/First.NOTE", small_note)
    bundle.writestr("Course/broken.ntb", b"broken note")
    bundle.writestr("__MACOSX/._ignored.note", small_note)
    bundle.writestr("../unsafe.note", small_note)
    bundle.writestr("readme.txt", b"Not a note")

with zipfile.ZipFile(root / "empty.zip", "w") as bundle:
    bundle.writestr("readme.txt", b"No notes here")
(root / "broken.zip").write_bytes(b"not a ZIP archive")
