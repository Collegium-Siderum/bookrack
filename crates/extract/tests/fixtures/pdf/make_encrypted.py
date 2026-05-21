"""Generate the encrypted PDF fixtures from a plain Typst-built PDF.

Typst cannot encrypt its output, so encryption is a post-processing
step. pikepdf (a binding over qpdf) applies it. Run this after
compiling prose_en.typ, whose PDF is used as the plaintext source.

  python make_encrypted.py

Two fixtures, covering the two cases the PDF adapter must tell apart:

  encrypted_userpw.pdf     A user (open) password is set. The file
                           cannot be opened without it; pdfium returns
                           a password error and the adapter maps that
                           to ExtractError::DrmProtected.

  encrypted_restricted.pdf Only an owner password is set, so the file
                           opens with no password but is permission
                           restricted. Built at revision 6 (AES-256) on
                           purpose: that is the security-handler
                           revision pdfium-render's enum cannot
                           classify, so the adapter must fall back to
                           "encrypted, handler revision unknown".
"""

import pikepdf

SRC = "prose_en.pdf"

with pikepdf.open(SRC) as pdf:
    pdf.save(
        "encrypted_userpw.pdf",
        encryption=pikepdf.Encryption(user="open-sesame", owner="open-sesame", R=6),
    )

with pikepdf.open(SRC) as pdf:
    pdf.save(
        "encrypted_restricted.pdf",
        encryption=pikepdf.Encryption(
            user="",
            owner="keep-out",
            R=6,
            allow=pikepdf.Permissions(
                extract=False,
                modify_other=False,
                print_highres=False,
            ),
        ),
    )

print("wrote encrypted_userpw.pdf and encrypted_restricted.pdf")
