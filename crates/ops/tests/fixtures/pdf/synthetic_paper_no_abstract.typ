// A synthetic paper fixture whose first two pages carry no text.
//
// The abstract pick reads an anchored heading, then falls back to the
// text of the first two pages. Neither exists here: no heading names
// an abstract and the only text sits on the third page, so the paper
// is extracted, structured, and audited, and has nothing to embed.
//
// Regenerate with:
//
//     typst compile synthetic_paper_no_abstract.typ synthetic_paper_no_abstract.pdf

#set page(width: 8.5in, height: 11in, margin: 0.8in)
#set text(font: "New Computer Modern", size: 11pt)

#pagebreak()

#pagebreak()

Synthetic Notes on an Empty Opening

These synthetic notes begin on the third page of a deliberately
fictional document. The first two pages are blank, so a reader that
looks for an opening statement on them finds no text at all, and the
notes that follow are plain body paragraphs about nothing in
particular.

A second paragraph keeps the page from being a single block. It
describes a tractable problem with one transparent trick, in words
chosen only to give the text layer something to hold.
