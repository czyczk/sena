# Tasks

- [x] 1. sena-core: constants (HF split 15600 Hz, 64k HF track, 256k layout
      threshold), `account3`, profile tag parse/format (`300@15600`), tests
- [x] 2. sena-dsp: `FIR_15600` table + generator, `CrossoverStream`/`split`
      selection, complementary-reconstruction and stream/batch equivalence
      tests
- [x] 3. sena-enc: three-way streaming pipeline, three parallel codec
      subprocesses, `A_OPUSHF` mux, audio SHA-256 v2 (3 streams)
- [x] 4. senaenc CLI: `--opus-topband-stereo`, three-track accounting/
      usage text, topband default rules ([192,256) senav => opus kbps)
- [x] 5. sena-dec demux + probe + whole-file pipeline: `A_OPUSHF`
      validation, profile tag parsing, three-track decode/mix/bit-accounting
- [x] 6. sena-dec streaming decoder: third track state, chunking, seek,
      per-track pre-skip; senadec `--dump-tracks` gains the top track
- [x] 7. e2e asset (256k senav three-track) + test updates
      (streaming-vs-whole-file, probe validation), notes updates
- [x] 8. parallelism pass: concurrent per-child drain in `run_parallel`,
      LF/HF band branches on scoped threads in three-track chunks (output
      verified bit-identical), whole-file decode decodes the three tracks
      concurrently; documented what stays sequential (streaming decoder,
      codec-vs-DSP overlap)
