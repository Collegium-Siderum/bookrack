# PDF test fixtures

The PDF adapter's test corpus: a focused set of synthetic, born-digital
PDFs plus the degenerate cases the adapter must handle without a clean
text layer. No real book content appears here — repository discipline
forbids it, and synthetic fixtures keep every expectation auditable.

## The fixture model

A PDF is an opaque binary, so each text-bearing fixture is kept as a
**pair**:

- a `.typ` (or, for the encrypted and image fixtures, a generating
  script) — the human-auditable source a reviewer reads;
- the compiled `.pdf` — the binary a test actually loads.

The source is the auditable form; the PDF is a build artifact checked
in beside it. This mirrors how the EPUB fixtures keep an unzipped,
readable directory next to the archive the test zips at runtime. PDFs
are **not** compiled at test time: that would make Typst, Pillow, and
fonts a build dependency, which the project deliberately avoids.
Compile once, commit the binary.

The one exception is `corrupt.pdf`: it is tiny and pure ASCII, so the
file itself is the auditable form and is written by hand.

## Regeneration

Tools used (pin these when regenerating):

- Typst 0.14.2 — the `.typ` fixtures
- pikepdf 10.3 (a binding over qpdf) — the encrypted pair
- Pillow 12.1 — the image-only fixture
- fonts: Libertinus Serif (bundled with Typst), Source Han Serif SC
  (system) for CJK

```text
typst compile prose_en.typ          # and prose_cjk, two_column,
                                    # toc_deep, biblio_garbage
python make_encrypted.py            # derives the two encrypted PDFs
                                    # from prose_en.pdf
python make_image_pdf.py            # draws the image-only PDF
```

`corrupt.pdf` is written by hand, not generated — see its row below.

Document dates are pinned in every `.typ` source, so `/Info`
CreationDate is a known value a test can assert on and regeneration
stays stable.

## The fixtures

| File | Pages | Exercises |
| --- | --- | --- |
| `prose_en` | 3 | Single-column English prose; headings at depth 1..3 feeding the `/Outline`; a footnote; a running header and page numbers (layout pollution); clean `/Info` biblio. |
| `prose_cjk` | 2 | Single-column CJK prose, ragged. Pins extractor behaviour on ideographic text. |
| `two_column` | 2 | Full-width title and abstract above a two-column body; Latin and CJK columns; reading order must run down the left column then the right, across a page break. |
| `toc_deep` | 4 | A four-level `/Outline` (18 entries); depth must survive, and anchoring must spread across pages instead of collapsing onto one block. |
| `biblio_garbage` | 2 | Deliberately unreliable `/Info`: a Word working-file name as title, an account name as author, a production date unrelated to the publication year. The trustworthy bibliography is on a faux title page in the body. |
| `encrypted_userpw` | — | A user (open) password is set; the file cannot be opened without it. Expect `ExtractError::DrmProtected`. |
| `encrypted_restricted` | 3 | Owner password only (opens with no password) at revision 6 / AES-256. It is `prose_en.pdf` re-saved with encryption, so its extracted content is identical to `prose_en`. |
| `image_only` | 1 | A single raster page with no text layer at all. There is nothing to extract, so the adapter must route it to OCR — `ExtractOutcome::NeedsOcr`. |
| `corrupt` | — | The `%PDF` header followed by a trailer whose `/Root` names a non-existent object, a `startxref` offset past end-of-file, and no object bodies at all. PDFium's cross-reference recovery finds nothing to build a document from, so the load fails: `ExtractError::CorruptFile`. |

A clean-`/Info` case is covered by `prose_en`; no separate fixture for
it.

The CJK fixtures — `prose_cjk` and the CJK section of `two_column` —
are set ragged, not justified. Typst's CJK justification widens
inter-character gaps that PDFium misreads as spaces; a real
born-digital Chinese corpus, checked for this, shows no such artifact
(its justified running prose extracts clean). Ragged setting keeps the
fixture faithful to how real CJK PDFs extract. `prose_cjk.typ` carries
the full note.

## Known extractor behaviour on these fixtures

Recorded so the structural test assertions make sense — these are the
adapter's present behaviour, not fixture defects:

- **Headings are not classified.** A PDF has no semantic headings;
  every block is `Body`. Heading structure reaches downstream through
  the `/Outline` TOC, not `BlockKind::Heading`.
- **Footnotes are `Body`.** `prose_en`'s footnote extracts as text but
  is not labelled `Footnote`.
- **Paragraphs are reconstructed from glyph coordinates.** Columns are
  read in order, left then right; a full-width element above a
  two-column body reads before the columns; running headers, footers,
  and page numbers are dropped. Two limitations remain: a paragraph
  that continues across a column break is reported as two blocks, one
  per column; and a running header is dropped only where it appears as
  its own line on two or more pages, so a page on which reconstruction
  merged it into the body text keeps it.

## Not yet covered

- Adversarial layout — a generator that splits lines into pathological
  segment runs (seen in some Calibre output). Clean engines like Typst
  do not reproduce it, so it cannot be captured by a synthetic fixture;
  it belongs with real-corpus checking, out of scope for this corpus.
