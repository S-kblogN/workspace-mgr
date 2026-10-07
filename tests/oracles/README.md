# Legacy storage compatibility oracles

These four Python adapters preserve the pre-0.7 storage protocol for isolated
regression comparisons. They are development fixtures: the Rust executable
neither embeds nor invokes them, and the crate/native release packages exclude
them. Python test clients use temporary repositories and simulated S3 only.

Production storage lives in `src/native_engine.rs`, `src/native_s3.rs`,
`src/native_versions.rs`, and `src/native_archive.rs`.
