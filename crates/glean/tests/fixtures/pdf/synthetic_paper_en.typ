// A synthetic conference-paper fixture for the glean pipeline.
//
// The fixture must not resemble any real paper: title, authors, venue
// and DOI are all clearly synthetic. The shape, on the other hand, is
// representative — Abstract heading, a body paragraph long enough to
// satisfy the abstract-picking heuristics, and a CSL-style metadata
// block at the end so the IDENTIFY pass has something to recognize.
//
// Regenerate with:
//
//     typst compile synthetic_paper_en.typ synthetic_paper_en.pdf
//
// Typst 0.14.2 pins the binary the rest of the workspace assumes.

#set page(width: 8.5in, height: 11in, margin: 0.8in)
#set text(font: "New Computer Modern", size: 11pt)

#align(center)[
  #text(size: 16pt, weight: "bold")[
    Synthetic Findings in Test Spaces
  ] \
  #v(0.4em)
  #text(size: 11pt)[
    First Author #h(0.5em) Second Author \
    Synthetic Institute, Nowhere
  ]
]

#v(1em)

#align(center)[
  #text(weight: "bold")[Abstract]
]

This synthetic abstract describes a deliberately fictional study of
test spaces. We outline a tractable problem with a single, transparent
trick and confirm the result with synthetic data. The contribution is
narrow on purpose: a single-paragraph abstract long enough to satisfy
the abstract-picking heuristic and short enough to fit one chunk
under the default chunk-length target.

== 1. Introduction

Synthetic content stands in for any real introduction. The body of
this paper carries placeholder paragraphs only; the pipeline reads
the abstract above and the metadata footer below.

== 2. Method

We carry no method. The fixture exists to drive the glean pipeline
end-to-end against a born-digital PDF whose text layer is clean.

== 3. Results

We report no results. See the abstract for the contribution.

== 4. Discussion

This section is intentionally empty.

#v(1em)
#line(length: 100%)

#text(size: 9pt)[
  Proceedings of the Synthetic Conference, 2020. \
  DOI: 10.5555/synthetic.0001 #h(1em) arXiv:0000.00001 [cs.XX] 1 Jan 2020
]
