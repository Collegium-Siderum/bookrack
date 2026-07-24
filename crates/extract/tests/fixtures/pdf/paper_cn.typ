// FIXTURE: Chinese-language journal article, the paper-metadata case.
//
// Exercises: the Chinese-form abstract anchor (a "zhai yao" heading
// followed by a body long enough to clear the anchored-body minimum),
// a keywords line that terminates the abstract window, a full-width
// DOI banner that the metadata-text fold must map to ASCII, and a
// references section whose heading must terminate the metadata-scan
// window before the bibliography entries.
//
// CJK is set ragged (justify: false) so the compiled PDF carries no
// pdfium space artifacts between ideographs; see prose_cjk.typ.
//
// Regenerate:  typst compile paper_cn.typ

#set document(
  title: "海岸线问题的中文样例",
  author: "书架虚构作者组",
  date: datetime(year: 2024, month: 3, day: 12),
)

#set page(paper: "a4", margin: 2.4cm)
#set text(font: ("Libertinus Serif", "Source Han Serif SC"), size: 10.5pt)
#set par(justify: false, leading: 0.68em)

#align(center, text(size: 16pt, weight: "bold")[海岸线问题的中文样例])
#v(0.4em)
#align(center, text(size: 10pt)[书架虚构作者组 · 样例研究所])
#v(0.4em)
// Full-width DOI banner: every glyph below sits in the U+FF01..U+FF5E
// block and must fold to plain ASCII in the metadata-scan text.
#align(center, text(size: 9pt)[ＤＯＩ：１０．１２３４／ｂｋｒ．２０２４．００７５３])
#v(0.2em)
#align(center, text(size: 9pt)[Journal of Synthetic Coastlines 12 (2024) 100–115])
#v(1em)

#show heading: it => {
  set text(size: 12pt, weight: "bold")
  block(above: 1em, below: 0.6em, it)
}

= 摘要

海岸线的长度并不是一个固定的数值。用来测量它的尺子越短，得到的数字就越大，因为短尺能够进入长尺一步跨过的每一处湾汊与岬角。本文以中文样例重述这一问题，并给出它唯一的实际推论：任何海岸线长度，只有连同测量尺度一起报告，才是可以被他人核对的记录。一个不带尺度的长度数字，不应被当作不够精确，而应被当作空的。

*关键词*：海岸线；尺度；测量；可核对性

= 引言

把海岸线问题换成中文来叙述，结论并不因语言而改变。海岸在每一个尺度上都向观察者递出新的细节，于是任何一次测量都只是某把尺子沿岸走过的路径长度。要让两次测量可以比较，唯一的办法是把尺度写在数字旁边。

= 方法

本文不做新的测量，只把已有的论证整理成可供自动化测试使用的最小样例。凡是需要标识符的地方，一律使用虚构的期刊名与登记号，以免与任何真实出版物混淆。

= 参考文献

#set par(first-line-indent: 0pt)

[1] Bibliographia Prima. On Synthetic Citations. Journal of Nowhere, 1999.

[2] 虚构文集编委会：《关于尺度的注记》，样例出版社，2001。
