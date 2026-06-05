---
name: Bug report
about: Report a defect in bookrack.
labels: bug
---

## What happened?

<!-- Describe the observed behaviour. -->

## What did you expect?

<!-- Describe the expected behaviour. -->

## Reproduction

<!--
Steps that trigger the problem.

A small reproducer that runs against a fresh data dir is the most
helpful kind of repro. If the bug only shows up against your live
library, that is fine — say so, and use the diagnose bundle below to
share the forensic context.
-->

## Diagnose bundle

Please run:

```
bookrack diagnose
```

and attach the resulting `.tar.gz` to this issue. The bundle is
scrubbed by default — paths and CJK book titles are replaced with
placeholders before packaging. Inspect the archive before attaching if
you want to verify the redaction; pass `--no-scrub` to skip it.

## Environment

- `bookrack` version (`bookrack info`):
- Operating system and version:
- Rust toolchain (if you built from source): `rustc --version`
