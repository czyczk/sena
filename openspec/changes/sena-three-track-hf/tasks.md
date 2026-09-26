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
- [x] 9. top track rework: direct 48 kHz coding replaced by SSB shift to
      baseband + 16 kHz stream (sena-dsp Hilbert/ShiftStream + tests,
      encoder chain, decoder 16k->48k upsample + shift-up in whole-file and
      streaming paths incl. seek context back-off, container rate 16000,
      e2e asset regenerated). Also fixed a latent streaming-decoder bug the
      new chunk pattern exposed: the LF resampler window drain must stay on
      the decimation grid (a 3/2-ratio drain by an odd core count shifted
      all later LF output by half a frame).
