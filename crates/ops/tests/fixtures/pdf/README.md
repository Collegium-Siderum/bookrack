# Paper test fixtures

Synthetic PDFs for the glean pipeline. The fixture model mirrors the
extract crate's PDF fixtures:

- `.typ` is the human-auditable source a reviewer reads;
- `.pdf` is the compiled binary the test actually loads.

PDFs are committed beside their sources rather than compiled at test
time, so the project deliberately keeps Typst, Pillow and fonts out
of the build dependency set.

## Files

- `synthetic_paper_en.typ` / `.pdf` — a one-page conference paper with
  an `Abstract` heading, a `Proceedings of …` footer, a sample DOI
  block and an arXiv tag. Identifiers in the footer are all from the
  reserved sample-only blocks (`10.5555/…`, `arXiv:0000.00001`); no
  real paper is reproduced or referenced.

## Regeneration

Tools used (pin these when regenerating):

- Typst `0.14.2`

```sh
typst compile synthetic_paper_en.typ synthetic_paper_en.pdf
```

Re-run after editing the `.typ` source. The PDF must be committed
alongside the source so a CI checkout has the binary available
without a Typst install.
