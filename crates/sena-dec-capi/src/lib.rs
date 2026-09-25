//! Empty facade: the `#[no_mangle]` C ABI in `sena-dec` is re-exported into
//! this cdylib (libsena_dec.so / sena_dec.dll) for runtime-loaded hosts.
#![allow(unused_imports)]
pub use sena_dec::*;
