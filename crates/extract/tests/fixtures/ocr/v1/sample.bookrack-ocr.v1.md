---
schema: 1
engine: polyocr/glm
engine_version: 0.0.0-synthetic
preset: en-academic
dpi: 180
---

<!-- page 1 (sheet 1) -->

Synthetic OCR Fixture

A round-trip anchor for the OCR adapter

<!-- page 2 (sheet 2) -->

Chapter One

The first paragraph of the first chapter. Plain English prose with no
special characters; the sample exists to anchor the round-trip assertions
in tests, not to look like a real book.

A second paragraph follows on the same physical page.

<!-- page 3 (sheet 3) -->

The third page continues the chapter. There is no outline entry for this
page; the parser must place the body block here under the existing chapter,
with the outline anchored at the chapter's first block on page two.
