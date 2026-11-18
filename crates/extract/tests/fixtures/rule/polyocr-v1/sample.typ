// FIXTURE: 3-page synthetic PDF, the source the page-marked text beside
// it stands for.
//
// Tests name it as the scan a rule's text was produced from, and read
// its page count. The body text is deliberately short and trivial.
//
// Regenerate:  typst compile sample.typ

#set document(
  title: "Synthetic OCR Fixture",
  author: "Bookrack Tests",
  date: datetime(year: 2024, month: 1, day: 1),
)

#set page(paper: "a5", margin: 2cm)
#set text(font: "Libertinus Serif", size: 11pt)
#set par(leading: 0.7em)

#show heading.where(level: 1): it => {
  pagebreak(weak: true)
  set text(size: 18pt, weight: "bold")
  block(below: 1em, it)
}

// --- Page 1: title page (no outline entry).
#v(3cm)
#align(center)[
  #text(size: 22pt, weight: "bold")[Synthetic OCR Fixture]
  #v(0.6em)
  #text(size: 12pt, style: "italic")[A round-trip anchor for the OCR adapter]
]

// --- Page 2: chapter one, the single outline entry.
= Chapter One

The first paragraph of the first chapter. Plain English prose with no
special characters; the sample exists to anchor the round-trip assertions
in tests, not to look like a real book.

A second paragraph follows on the same physical page.

// --- Page 3: continuation, no new heading.
#pagebreak()

The third page continues the chapter. There is no outline entry for this
page; the parser must place the body block here under the existing chapter,
with the outline anchored at the chapter's first block on page two.
