// FIXTURE: English single-column prose.
//
// Exercises: body readability and reading order, headings at depth
// 1..3 feeding the PDF /Outline, a footnote, a running header and page
// numbers (the layout-pollution case), clean /Info biblio. Run in both
// --pdf-paragraphs modes to see line-heuristic (page lumps) vs coords
// (real paragraphs).
//
// Regenerate:  typst compile prose_en.typ

#set document(
  title: "The Printed Page",
  author: "Bookrack Fixture Authors",
  date: datetime(year: 2011, month: 9, day: 20),
)

#set page(
  paper: "a5",
  margin: (x: 2cm, top: 2.4cm, bottom: 2.4cm),
  numbering: "1",
  header: {
    set text(size: 8pt, style: "italic")
    align(right)[The Printed Page]
  },
)

#set text(font: "Libertinus Serif", size: 10pt)
#set par(justify: true, first-line-indent: 1.2em, leading: 0.7em)
#set heading(numbering: "1.1")

#show heading.where(level: 1): it => {
  pagebreak(weak: true)
  set text(size: 15pt, weight: "bold")
  block(below: 1.1em, it)
}

= The Geometry of the Margin

== Why Pages Have Borders

A margin is not wasted paper. It is the frame that lets the eye know
where a line begins and where it safely ends, and without it a reader's
gaze slides off the text block and loses its place. Early scribes,
working before the conventions of the printed book had settled, left
margins so generous that the written area occupied barely half the
sheet. They were not being wasteful; they were buying legibility, and
they were leaving room for the reader's own hand to answer back.

The proportions that govern a well-set page were worked out long before
anyone could state them as arithmetic. A page feels balanced when the
outer margin is wider than the inner one, because two facing pages share
their inner margins and the eye reads the pair as a single spread. Make
the inner margin too wide and the spread splits in two; make it too
narrow and the text vanishes into the binding. The rule is invisible
when it is obeyed and glaring the moment it is broken.

=== A Note on Gutters

The gutter is the inner margin measured across the bound edge, and it is
the one dimension a designer cannot recover after the fact. A book sewn
too tightly buries its gutter text in the curve of the spine, and no
amount of careful typesetting on the flat page can rescue a line the
reader cannot physically flatten into view.

== The Size of the Type

Type that is too small tires the eye within a page; type that is too
large turns a paragraph into a staircase the reader must climb. The
comfortable range is narrower than most people expect, and it depends
less on the height of the letters than on the length of the line they
form.#footnote[A line of roughly sixty to seventy characters is the
figure most often cited; the body of the line, not the body of the
letter, is what governs comfort.] A long line forces the eye to travel
so far that it struggles to find the start of the next one.

This is why a wide page is so often set in two columns rather than one.
The column is not a decorative choice but a correction: it cuts an
unreadably long measure into two readable ones. The cost is a second
boundary the eye must cross, and a reader pays that cost gladly because
the alternative is worse.

= The Life of a Book

A book is read far less often than it is shelved, and a binding that
survives a century of shelving has done most of its job. The boards
guard the text block from light and dust; the spine takes the wear of
every removal and return. A book that is beautiful but fragile is a book
that will be admired and not used, and a working library has little
patience for objects it cannot open.

The paper itself is the slowest clock in the room. Rag paper, made
before the industrial era, ages gently and can outlast the building that
houses it. The wood-pulp paper that replaced it carries its own acid and
slowly burns itself brown from the inside. A reader who opens a book
from either era is, without thinking about it, reading the chemistry of
the page as much as the words upon it.
