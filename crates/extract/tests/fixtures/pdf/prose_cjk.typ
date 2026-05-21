// FIXTURE: CJK single-column prose.
//
// Exercises: CJK extraction and reading order, headings feeding the
// /Outline, paragraph reconstruction on ideographic text.
//
// NOT justified, on purpose. Real Chinese books are justified, but
// Typst's CJK justification widens the inter-character gaps, and pdfium
// then reads some of those gaps as spaces. That artifact does not occur
// on real born-digital Chinese PDFs — checked against a real corpus,
// whose justified running prose extracts with no stray inter-ideograph
// spaces at all. A justified Typst fixture would bake a Typst-only
// artifact into the expected output and misrepresent how CJK extracts;
// ragged setting keeps the fixture faithful to real extraction.
//
// Each CJK paragraph is ONE physical source line: Typst turns a single
// markup newline into a space, and between ideographs that space would
// survive into the PDF text.
//
// Regenerate:  typst compile prose_cjk.typ

#set document(
  title: "纸与墨的简史",
  author: "丛书测试编者",
  date: datetime(year: 2016, month: 4, day: 8),
)

#set page(
  paper: "a5",
  margin: (x: 2cm, top: 2.4cm, bottom: 2.4cm),
  numbering: "1",
  header: {
    set text(size: 8pt, style: "italic")
    align(right)[纸与墨的简史]
  },
)

#set text(font: ("Libertinus Serif", "Source Han Serif SC"), lang: "zh", size: 10.5pt)
#set par(justify: false, first-line-indent: 2em, leading: 0.9em)
#set heading(numbering: "1.1")

#show heading.where(level: 1): it => {
  pagebreak(weak: true)
  set text(size: 15pt, weight: "bold")
  block(below: 1.1em, it)
}

= 纸的来历

== 从简牍到纸张

在纸出现之前，文字寄身于沉重的载体之上。竹简要一根根削平、钻孔、以绳编连，写满一部书往往要装满整整一车，搬运它便是一桩力气活。木牍稍轻，却仍是以体积换取耐久。这些载体都有一个共同的难处：它们记录文字的能力，被自身的重量牢牢限制住了。

纸的发明把这道难处一笔勾销。它把书写的表面从一种「物件」变成了一种「平面」，轻、薄、可叠、可卷。一旦表面不再昂贵，文字便可以铺张，可以反复誊抄，可以为了让后人读懂而留出宽阔的天地。载体的轻省，最终改变的是知识传播的速度。

=== 关于纤维

纸的耐久，藏在它的纤维里。以破布制成的纸，纤维长而坚韧，历经数百年仍不脆裂；以木浆制成的纸，纤维短，又常带着未除尽的酸，于是从内部慢慢发黄、变脆。一张纸能活多久，在它被抄上字之前，其实就已经定下了。

== 墨的脾气

墨的好坏，要过很久才显出来。新写的字，浓墨与淡墨看上去相差无几；可一旦经年累月，劣墨会褪、会洇、会在潮气里化开，而好墨却像是与纸长在了一起。写字的人未必看得见这个差别，藏书的人却迟早要替他承受这个差别。

正因如此，讲究的抄书人对墨的挑剔，几乎不亚于对纸。他们明白自己写下的并不只是当下的一行字，而是一份要交到许多年以后某位陌生读者手里的东西。那位读者看不到此刻的灯、此刻的手，他能依靠的，只有纸与墨本身。

= 书的形制

册页的形制，是为翻阅而生的。卷轴要从头展到尾，找一句话便要把半卷书摊开；册页却可以随手翻到任意一页，又随手合上。这一点之差，决定了哪一种书更适合被反复查考，而不只是被一字一句地通读。
