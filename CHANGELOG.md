# Changelog

## 0.1.1 — 2026-10-22

Security-maintenance release. No behavior changes; update to pick up fixed TLS and networking dependencies.

- The TLS trust-store and handshake crates are updated to rustls 0.23.46, rustls-webpki 0.103.16 and webpki-roots 1.0.10, which pick up the fixes for the eMudhra CA incident (RUSTSEC-2026-0346 to -0349 in rustls; RUSTSEC-2026-0381, -0382 and -0383 in rustls-webpki and webpki-roots). scout's HTTPS calls to the model provider use these.
- quinn-proto moves to 0.11.18 (RUSTSEC-2026-0338 to -0342) and crossbeam-epoch to 0.9.20 (RUSTSEC-2026-0204), both transitive dependencies.
- `cargo audit` reports none of these advisories against 0.1.1.

## 0.1.0 — 2026-07-06

First public release.

- Corpus indexing (`scout index`) with two-tier cards, hash-keyed incremental rebuilds, and atomic generation snapshots.
- Session-start orientation (`scout brief`) with module map, entry points, and unsupported-file coverage reporting.
- Natural-language corpus query (`scout "<query>" <dir>`) with a quote-verified extraction firewall — every delivered finding is machine-checked against the source file.
- `capabilities`, `schema`, and `doctor` self-description commands.
- Code (M1) and policy-markdown (M3) eval suites: 100% candidate-file recall, coverage above the predeclared gate bar, 0 poison survivors, negatives clean on both milestones.
- PDF and DOCX read adapters (doctor-verified, not yet corpus-evaled).
