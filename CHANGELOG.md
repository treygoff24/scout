# Changelog

## 0.1.0 — 2026-07-06

First public release.

- Corpus indexing (`scout index`) with two-tier cards, hash-keyed incremental rebuilds, and atomic generation snapshots.
- Session-start orientation (`scout brief`) with module map, entry points, and unsupported-file coverage reporting.
- Natural-language corpus query (`scout "<query>" <dir>`) with a quote-verified extraction firewall — every delivered finding is machine-checked against the source file.
- `capabilities`, `schema`, and `doctor` self-description commands.
- Code (M1) and policy-markdown (M3) eval suites: 100% candidate-file recall, coverage above the predeclared gate bar, 0 poison survivors, negatives clean on both milestones.
- PDF and DOCX read adapters (doctor-verified, not yet corpus-evaled).
