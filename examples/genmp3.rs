// genmp3.rs - helper: writes an MPEG-1 Layer III test file.
// Usage: cargo run --example genmp3 -- <outfile> [nframes] [mono] [huffman] [bitrate]
//
// By default a silent CBR file is written (zero main data). With `huffman` the
// main data carries real huffman-coded spectra (scalefactors, big values with
// linbits escapes, count1 quadruples), which exercises the whole pipeline.
// `bitrate` selects the MPEG-1 Layer III bitrate table index (1..14, default 9
// = 128 kbps).
//
// Used to produce sample inputs for byte-comparing the Rust CLI against the
// original C++ tool (and for manual debugging).

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: genmp3 <outfile> [nframes] [mono] [huffman] [bitrate]");
        std::process::exit(1);
    }
    let path = &args[1];
    let nframes: i32 = args.get(2).map_or(6, |s| s.parse().unwrap());
    let mono: bool = args.get(3).is_some_and(|s| s == "1" || s == "mono");
    let huffman: bool = args
        .get(4)
        .is_some_and(|s| s == "1" || s == "huff" || s == "huffman");
    let bitrate: i32 = args.get(5).map_or(9, |s| s.parse().unwrap());

    let mp3 = if huffman {
        packmp3::testsupport::build_huffman_mp3(0x6E97_DCBA, nframes, mono, bitrate)
    } else {
        // silent builder ignores the bitrate index and is fixed at 128 kbps
        assert_eq!(bitrate, 9, "silent mode is fixed at bitrate idx 9 (128 kbps)");
        packmp3::testsupport::build_silent_mp3(nframes, mono)
    };
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(&mp3).unwrap();
    println!(
        "wrote {} bytes ({} frames, {}, {}) to {}",
        mp3.len(),
        nframes,
        if mono { "mono" } else { "stereo" },
        if huffman { "huffman-coded" } else { "silent" },
        path
    );
}
