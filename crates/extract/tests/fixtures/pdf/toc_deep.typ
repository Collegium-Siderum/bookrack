// FIXTURE: a deep, multi-page table of contents.
//
// Exercises: the /Outline carries headings four levels deep; the
// extractor must preserve depth (0..3 after flattening) and anchor each
// entry to a block. The handbook runs several pages, so headings land
// on different pages and anchoring must spread across many blocks
// rather than collapsing onto block 0. Where several headings still
// share a page, page-granularity anchoring sends them to one block —
// the documented limitation, visible here on purpose.
//
// Regenerate:  typst compile toc_deep.typ

#set document(
  title: "A Handbook of Archive Practice",
  author: "Bookrack Fixture Authors",
  date: datetime(year: 2008, month: 7, day: 2),
)

#set page(paper: "a5", margin: 2cm, numbering: "1")
#set text(font: "Libertinus Serif", size: 10pt)
#set par(justify: true, first-line-indent: 1.2em, leading: 0.7em)
#set heading(numbering: "1.1.1.1")

#show heading.where(level: 1): it => {
  pagebreak(weak: true)
  set text(size: 14pt, weight: "bold")
  block(below: 1em, it)
}
#show heading.where(level: 2): it => {
  set text(size: 12pt, weight: "bold")
  block(above: 1em, below: 0.5em, it)
}

= Part One: The Building

== The Reading Room

=== Light

==== Direct Sunlight

Direct sun is the enemy a reading room is built to exclude. It fades
pigment, yellows paper, and warps the boards of a binding within a
single summer if a book is left in its path. Desks are therefore turned
away from the windows that admit it, and the shelves retreat from the
glass entirely. The light a reader needs and the light a book can
survive are not the same light, and the room is a compromise struck
between them.

==== Indirect Light

A clerestory window, set high in the wall and turned from the noon arc,
spills an even and indirect light onto the desk while sparing the
shelves the worst of the heat. It is the oldest answer to the lighting
problem and it remains the best. A room lit this way needs no lamp until
dusk, and the manuscripts shelved in it are spared the slow damage that
a sunlit room inflicts without anyone noticing.

=== Air

Still air lets mould settle and spores take hold; air that moves too
briskly dries bindings unevenly and lifts dust from every surface into
the reader's breath. The aim is a slow, steady exchange that no one in
the room can feel. Ventilation is judged a success precisely when it is
imperceptible, which makes it one of the hardest systems in the building
to tune and the easiest to neglect.

== The Stacks

=== Shelving

A book belongs upright, supported by its neighbours, bearing its weight
on its boards and never on the fragile edges of its pages. A shelf
filled only halfway lets its books lean, and a leaning book deforms
under its own weight over years until the spine no longer sits square. A
block placed at the end of a short row holds the books vertical and
costs nothing but the discipline to use it.

=== Climate

Cool and dry preserves; warm and damp destroys. The exact figures
matter less than their steadiness, for a collection suffers more from a
climate that swings than from one held at a slightly wrong but constant
value. Materials expand and contract as conditions move, and it is the
movement, repeated season after season, that loosens joints and cracks
leather. A stable wrong number is kinder than a correct average reached
by veering above and below it.

= Part Two: The Collection

== Handling

=== Hands

Clean, dry hands serve a book better than cotton gloves, which dull the
sense of touch and catch on the very paper they are meant to protect. A
reader who can feel the page turns it with the right amount of force and
no more. The instinct to reach for gloves is understandable, but for
ordinary paper it trades a real benefit for an imagined one.

=== Supports

An open book should rest in a cradle that holds its two boards at a
gentle angle, so that the spine is never forced to lie flat. A binding
opened flat on a hard table is being asked to bend in a direction it was
never built to bend, and each such opening spends a little of the life
left in the spine. The cradle is not a courtesy to the book; it is a
condition of consulting it without harm.

== Repair

=== Reversibility

==== Adhesives

Every adhesive introduced in a repair must be removable by a later
conservator without harm to the page beneath it. A repair that cannot be
undone is not a repair but damage deferred, because it forecloses the
choices of everyone who handles the book afterward. The conservator
works in materials chosen as much for how cleanly they come away as for
how well they hold.

==== Documentation

Each repair is recorded in plain terms: what was done, with which
materials, and on what date. The note outlives the conservator who wrote
it and speaks directly to whoever opens the book next, perhaps a century
on. An undocumented repair leaves that future reader guessing whether a
feature of the book is original or an intervention, and the guess can go
wrong in either direction.

=== Knowing When to Stop

The most disciplined repair is often the one not attempted at all. A
book that is stable, even if visibly worn, is frequently best left
exactly as it is, because every intervention carries its own small risk
and its own loss of original material. A conservator's restraint is as
much a skill as any technique of the hand, and it is the harder skill to
teach, since it shows only in the work not done.
