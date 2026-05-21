// FIXTURE: two-column layout, the column reading-order test.
//
// Exercises: a program must reconstruct reading order from coordinates
// — down the left column to its foot, then back to the head of the
// right, and on across a page break. A column boundary the extractor
// fails to detect interleaves the two columns into nonsense. A
// full-width title and abstract sit above the two-column body, so the
// extractor must also handle the switch from one flow to two. The body
// is long enough to fill both columns and run onto a second page, and
// it ends with a CJK section so column detection is tested on
// ideographic text too.
//
// Regenerate:  typst compile two_column.typ

#set document(
  title: "On the Measurement of Coastlines",
  author: "Bookrack Fixture Authors",
  date: datetime(year: 2014, month: 1, day: 30),
)

#set page(paper: "a4", margin: 2.2cm, numbering: "1")
#set text(font: ("Libertinus Serif", "Source Han Serif SC"), size: 10pt)
#set par(justify: true, leading: 0.65em, first-line-indent: 1em)

// Full-width masthead: title and abstract span both columns.
#align(center, text(size: 17pt, weight: "bold")[
  On the Measurement of Coastlines
])
#v(0.4em)
#align(center, text(size: 9pt, style: "italic")[
  A specimen article in two columns
])
#v(0.8em)

#block(inset: (x: 1.6cm), {
  set text(size: 9pt)
  set par(first-line-indent: 0pt)
  [*Abstract.* The length of a coastline is not a fixed quantity. It
  grows as the ruler used to measure it shrinks, because a shorter ruler
  follows inlets and headlands that a longer one steps straight across.
  This article states the problem plainly and draws out its single
  practical consequence: a measured length is meaningless unless the
  scale of measurement is reported beside it.]
})
#v(0.6em)

#show heading: it => {
  set text(size: 11pt, weight: "bold")
  block(above: 1em, below: 0.5em, it)
}

#columns(2, gutter: 1.1cm)[

= Introduction

Ask how long a coastline is, and the honest answer is another question:
measured with what? Lay a long ruler against a rugged coast and it
bridges every bay, cutting straight across water that the coast itself
curves around. The measured length comes out short. Take a shorter
ruler and it dips into those same bays, tracing a longer path along the
very same shore.

The length, in other words, depends on the ruler. This is not a flaw in
the instrument and not an error of the surveyor. It is a property of the
coast, which offers fresh detail at every scale at which one troubles to
look. A pebble on the beach repeats, in miniature, the ragged outline
of the bay that holds it, and the bay repeats the outline of the gulf.

The question has no answer until the ruler is named. Once it is, the
answer is exact, and it is exact only for that ruler. Change the ruler
and the answer changes with it, lawfully and predictably, in a single
direction: shorter rulers always report longer coasts.

= The Ruler and the Coast

Imagine walking the coast with dividers set to one kilometre, stepping
each span heel to toe and counting. Then walk it again with the dividers
set to one hundred metres. The second walk does not retrace the first.
It rounds promontories the first walk cut off and enters inlets the
first walk ignored, and it returns a larger count of a smaller span.

Halve the span again and the same thing happens again. There is no
setting so fine that further halving stops adding length, because the
coast keeps presenting structure below whatever scale has been reached.
The total does not converge on a final figure the way the steps of a
well-behaved measurement should. It simply keeps climbing.

This is what it means to say a coastline has no single length. The
phrase is not a paradox and not a trick of words. It is a plain
description of what the dividers do.

= The Consequence for Surveys

A surveyor who reports a single number for a coastline has, without
announcing it, also chosen a ruler. Two surveys that disagree may both
be correct, because each measured a real thing — its own ruler's path
along a shore that has no one true length to disagree about.

The remedy is not a finer instrument but an honest report. State the
scale, and the number becomes meaningful at once: it describes the coast
as seen at that scale, and a later reader can compare it with any other
measurement taken at the same one. Drop the scale, and the number
describes nothing that anyone can check.

Good practice, then, is simple to state. Never report a length without
the scale that produced it. Treat a length quoted without its scale not
as imprecise but as empty, and ask for the ruler before you trust the
figure.

= A Worked Illustration

Consider a single stretch of rocky shore, perhaps a few kilometres of it
between two headlands. Measured from a map drawn at a coarse scale, the
stretch reads as a gentle curve, and a ruler laid along it returns a
modest figure. The map has already done some of the smoothing: it could
not show the inlets, so the inlets are not there to be measured.

Now measure the same stretch from a survey drawn at a finer scale. The
inlets reappear, each with its own smaller inlets, and the ruler must
follow them all. The figure climbs, and it climbs by a margin too large
to dismiss as the error of the earlier measurement. Both figures are
correct; they describe the same shore seen through rulers of different
length.

Walk the shore in person, with dividers in hand, and the figure climbs
again. The body of the walker is now the ruler, and it is a short one.
It rounds every boulder and steps into every cleft, and it returns the
largest figure of the three. None of the three is the length of the
coast. Each is the length of a particular ruler's journey along it, and
that, the article has argued, is the only kind of coastal length there
is.

// CJK set ragged: a justified Typst CJK fixture would bake in a pdfium
// space artifact absent from real-book PDFs. See prose_cjk.typ.
#set par(justify: false)

= 中文小节：海岸与尺子

把同样的问题换成中文来说，并不会让它变得简单。海岸线的长度随尺子而变，尺子越短，量得的数字越大，因为短尺会钻进长尺一跨而过的每一道湾汊。这不是测量的失误，而是海岸本身的脾气：它在每一个尺度上都向观察者递出新的细节。

因此，一个只报数字、却不报测量尺度的海岸线长度，是无法被另一个人核对的。要让这个数字重新变得有意义，就必须把尺度一并写明。它描述的是「在这个尺度下看到的海岸」，而不是某个并不存在的、唯一为真的长度。

把尺度写在数字旁边，是一条简单的纪律。一个不带尺度的长度，不该被当作「不够精确」，而应被当作「空的」——在你信任它之前，先问清楚那把尺子。

]
