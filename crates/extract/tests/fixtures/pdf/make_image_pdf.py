"""Generate the image-only PDF fixture — a PDF with no text layer.

Typst always embeds real text, so it cannot produce the case the
quality gate must route to OCR: a page that is pure raster with no
extractable glyphs. This script draws a synthetic raster with Pillow
(deliberately NOT a real book scan — repository discipline forbids real
book content in fixtures) and saves it as a single-page PDF.

  python make_image_pdf.py

The result, image_only.pdf, carries one image object and zero text, so
the adapter must yield ExtractOutcome::NeedsOcr.
"""

from PIL import Image, ImageDraw

# A small page-shaped canvas; kept tiny so the committed fixture stays
# small. 72 dpi means the PDF page is about 3 x 4 inches.
WIDTH, HEIGHT = 220, 300

image = Image.new("RGB", (WIDTH, HEIGHT), "white")
draw = ImageDraw.Draw(image)

# Programmatic geometry standing in for a scanned page's ink: a border,
# a band of "text lines", and a "figure" box. There are no glyphs — the
# point is a page that looks like content but carries no text layer.
draw.rectangle([8, 8, WIDTH - 8, HEIGHT - 8], outline="black", width=2)
for row in range(11):
    y = 32 + row * 16
    draw.line([24, y, WIDTH - 24, y], fill=(70, 70, 70), width=3)
draw.rectangle([40, 220, WIDTH - 40, 280], outline="black", fill=(205, 205, 205))

image.save("image_only.pdf", "PDF", resolution=72.0)
print("wrote image_only.pdf")
