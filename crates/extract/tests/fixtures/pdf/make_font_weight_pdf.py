"""Generate the font-weight PDF fixture: one page, three paragraphs, each
set in its own non-embedded TrueType font.

PDFium reports a font's weight from its descriptor. When the descriptor
declares /FontWeight greater than 0, that value is used directly;
otherwise the weight is estimated from /StemV (StemV * 5 below 140,
StemV * 4 + 140 from 140 up). The three descriptors differ only in those
two keys, so each paragraph separates the two sources of the weight:

  Alpha    FixtureSansA  /FontWeight 700   /StemV 0    (estimate: 0)
  Bravo    FixtureSansB  /FontWeight 400   /StemV 140  (estimate: 700)
  Charlie  FixtureSansC  no /FontWeight    /StemV 140  (estimate: 700)

Every reported weight is an exact named step (0, 400, or 700). The font
names carry no "Bold", so nothing can read the weight from the name.
Each paragraph opens with its marker word and then repeats one fixed
sentence twice, so the text and its chunks do not depend on the fonts.

Standard library only:

  python make_font_weight_pdf.py

The /Info CreationDate is pinned, so regeneration is byte-stable.
"""

# Resource name, base font, /FontWeight (None leaves the key out), /StemV.
FONTS = [
    ("FA", "FixtureSansA", 700, 0),
    ("FB", "FixtureSansB", 400, 140),
    ("FC", "FixtureSansC", None, 140),
]
MARKERS = ("Alpha", "Bravo", "Charlie")
WORDS = (
    "the ledger keeps every entry in the order the clerk wrote it and "
    "each page carries a short note on where the next page begins"
).split()

FONT_SIZE = 11
LEADING = 14
PARAGRAPH_GAP = 42
LINE_CHARS = 64


def paragraph(marker):
    """The marker word and the sentence twice, wrapped at LINE_CHARS."""
    lines, line = [], [marker]
    for word in WORDS * 2:
        if len(" ".join(line + [word])) > LINE_CHARS:
            lines.append(" ".join(line))
            line = [word]
        else:
            line.append(word)
    lines.append(" ".join(line))
    return lines


def content_stream():
    """One text object per paragraph, top down from y = 760."""
    out, y = [], 760
    for (resource, _, _, _), marker in zip(FONTS, MARKERS):
        lines = paragraph(marker)
        out.append(f"BT /{resource} {FONT_SIZE} Tf {LEADING} TL 72 {y} Td")
        for i, text in enumerate(lines):
            out.append(("" if i == 0 else "T* ") + f"({text}) Tj")
        out.append("ET")
        y -= LEADING * len(lines) + PARAGRAPH_GAP
    return "\n".join(out).encode("ascii")


objects = []


def add(body):
    """Append an object body and return its 1-based object number."""
    objects.append(body)
    return len(objects)


catalog = add(None)
pages = add(None)

font_refs = []
widths = " ".join(["500"] * 95)  # FirstChar 32 .. LastChar 126
for resource, name, weight, stem_v in FONTS:
    font_weight = f" /FontWeight {weight}" if weight is not None else ""
    descriptor = add(
        f"<< /Type /FontDescriptor /FontName /{name} /Flags 32 "
        f"/FontBBox [0 -200 1000 900] /ItalicAngle 0 /Ascent 900 /Descent -200 "
        f"/CapHeight 700 /StemV {stem_v}{font_weight} >>".encode()
    )
    font = add(
        f"<< /Type /Font /Subtype /TrueType /BaseFont /{name} "
        f"/FirstChar 32 /LastChar 126 /Widths [{widths}] "
        f"/Encoding /WinAnsiEncoding /FontDescriptor {descriptor} 0 R >>".encode()
    )
    font_refs.append((resource, font))

content = content_stream()
stream = add(b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream")
font_dict = " ".join(f"/{resource} {number} 0 R" for resource, number in font_refs)
page = add(
    f"<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 612 792] "
    f"/Resources << /Font << {font_dict} >> >> /Contents {stream} 0 R >>".encode()
)
objects[pages - 1] = f"<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>".encode()
objects[catalog - 1] = f"<< /Type /Catalog /Pages {pages} 0 R >>".encode()
info = add(
    b"<< /Title (Font weight fixture) /Author (Bookrack Fixture Authors) "
    b"/CreationDate (D:20080702000000Z) >>"
)

pdf = bytearray(b"%PDF-1.7\n")
offsets = []
for number, body in enumerate(objects, 1):
    offsets.append(len(pdf))
    pdf += b"%d 0 obj\n" % number + body + b"\nendobj\n"
xref = len(pdf)
pdf += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objects) + 1)
for offset in offsets:
    pdf += b"%010d 00000 n \n" % offset
pdf += b"trailer\n<< /Size %d /Root %d 0 R /Info %d 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (
    len(objects) + 1,
    catalog,
    info,
    xref,
)

with open("font_weight.pdf", "wb") as f:
    f.write(pdf)
print("wrote font_weight.pdf")
