// packmp3 - Rust port of the packMP3 lossless MP3 compressor.
// Faithful port of the C++ code in source/ so that PMP output is byte-identical.

pub mod aricoder;
pub mod app;
pub mod bitops;
pub mod cli;
pub mod huffmp3;
pub mod hufftables;
pub mod mp3;
pub mod pmp;
pub mod state;
pub mod tables;
pub mod testsupport;
