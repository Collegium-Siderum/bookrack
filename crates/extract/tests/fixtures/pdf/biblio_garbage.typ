// FIXTURE: deliberately unreliable /Info metadata.
//
// Exercises Q4: a real PDF's /Info is roughly half garbage. The title
// is a Word working-file name, the author is an account name, and the
// CreationDate is the day the PDF was produced — not the publication
// year. The trustworthy bibliography lives in the body, on a faux title
// page, and disagrees with /Info on every field. The extractor must
// transcribe /Info faithfully, garbage and all; reconciling it against
// the page text is the METADATA stage's job, not extract's.
//
// Regenerate:  typst compile biblio_garbage.typ

#set document(
  // A Word working-file name leaking into the title slot.
  title: "Microsoft Word - chapter_revised_FINAL (2).docx",
  // An OS account name, not a person.
  author: "Administrator",
  // The production date. The book's real first-publication year (1962)
  // appears only on the title page below; the two must not be conflated.
  date: datetime(year: 2023, month: 11, day: 5),
)

#set page(paper: "a5", margin: 2.4cm)
#set text(font: "Libertinus Serif", size: 11pt)
#set par(justify: true, leading: 0.7em)

// --- Faux title page: the bibliography that /Info should have carried.
#v(3cm)
#align(center)[
  #text(size: 20pt, weight: "bold")[The Weather of the Archive]
  #v(0.6em)
  #text(size: 12pt, style: "italic")[Notes on Heat, Damp, and Time]
  #v(2cm)
  #text(size: 12pt)[Eleanor Hartwick]
  #v(3cm)
  #text(size: 10pt)[Marginalia Press]
  #v(0.3em)
  #text(size: 10pt)[First published 1962]
]

#pagebreak()

// --- A little body text, so the file is not an empty extraction.
#set par(first-line-indent: 1.2em)

An archive keeps two clocks. One is the calendar on the wall, which the
staff consult and obey. The other is slower and quieter: it is the
temperature and humidity of the rooms, and it is the clock the books
themselves keep. A reader notices the first clock and never the second,
yet it is the second that decides how long the collection will last.

The fixture you are reading exists to make a narrow point about
metadata. Everything a file says about itself in its embedded
properties may be wrong, and often is. The title above this paragraph,
the author named on the title page, and the year of first publication
are the trustworthy record. What the file's own properties report is a
working filename, an account, and a date of manufacture — three facts
about the document as an object, and none about the book as a work.
