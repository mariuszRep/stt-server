# Conventions

- Keep the single executable self-contained: native inference code links statically. The
  installed GPU driver and Windows system libraries are external platform dependencies.
- A model is trusted only after its immutable revision, size, SHA-256, and licence policy have
  been checked. Manually copied artifacts do not become installed models.
- First start and transcription have no download side effects. Selection and acquisition are
  explicit management actions.
- Preserve model-card facts, Handy editorial rank, and locally measured performance as
  separate provenance fields. Never present a source benchmark as a local measurement.
- Recommendations have a fixed curated order. Hardware probing is confined to model-load
  backend selection and observed diagnostics.
- Advertise a control only after verifying its model-side effect. Reject unsupported optional
  request fields. Clients omit unsupported or unknown controls rather than sending defaults.
- Bind to loopback by default, protect mutating and transcription routes with a bearer token,
  and keep long-running model operations observable across service restart.
- Build and test locally before adding dispatch-only candidate automation. A production
  release must promote the exact accepted candidate binary and recorded checksum.
