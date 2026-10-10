# Tasks

- [x] sena-core: MidCodec, xfi accounting (256-288, mid 128, hf 96->128),
      ES v3 (mid-codec byte; counted-AU mid block section), rational
      by-end AU placement for 44100 Hz, profile/tag/codec-name strings
- [x] sena-enc: ezcd-cli driver (discovery incl. WSL .exe lookup, wslpath
      path translation), mid 48k->44.1k resample with zero tail, s16 input
      (ezcd-cli f32 path mis-encodes at 44.1 kHz), assembly + mux
- [x] sena-dec: batch + streaming xHE mid lane (trim 1024 core, 44.1k->48k
      resample), seek via prev-indep mid AUs, demux index mid-AU metadata
- [x] decoder phase-exactness: chunk takes locked to the n=160 output grid
      + retained context kept a multiple of m=147 (streaming == whole-file
      to f32 precision)
- [x] golden: validate.py v3/xHE-mid aware; 600xfi@256 + 300o@160 +
      600o@288 PASS
- [x] fb2k plugin codec_profile; sena_dec.h mid_codec field; README;
      doctor entry
- [x] senadec --dump-tracks spot check on an xfi file
