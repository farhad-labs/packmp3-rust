// cli.rs - integration tests for the packmp3 command line binary.
//
// These run the actual `packmp3` executable (via CARGO_BIN_EXE) against
// synthetic MP3 files in a temporary directory and check the produced .pmp /
// .mp3 files as well as the round-trip behaviour, mirroring the C++ CLI flow.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::build_silent_mp3;
use packmp3::testsupport::build_huffman_mp3;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_packmp3")
}

/// create a fresh temp dir with a silent stereo MP3 file; returns (dir, mp3)
fn setup(dir_name: &str, mp3_name: &str, nframes: i32) -> (PathBuf, PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("packmp3_cli_{}", dir_name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let mp3_bytes = build_silent_mp3(nframes, false);
    let mp3_path = dir.join(mp3_name);
    fs::write(&mp3_path, &mp3_bytes).unwrap();
    (dir, mp3_path, mp3_bytes)
}

/// like `setup`, but writes a huffman-coded MP3 (real main data)
fn setup_huff(dir_name: &str, mp3_name: &str, seed: u32, nframes: i32) -> (PathBuf, PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("packmp3_cli_{}", dir_name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let mp3_bytes = build_huffman_mp3(seed, nframes, false, 9);
    let mp3_path = dir.join(mp3_name);
    fs::write(&mp3_path, &mp3_bytes).unwrap();
    (dir, mp3_path, mp3_bytes)
}

/// run the binary with the given args (cwd = dir); returns (status, stdout, stderr)
fn run(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run packmp3 binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// compress `mp3` in-process and return the `-v0` percentage the CLI prints,
/// using the exact same code path (and float formatting) as the binary
fn expected_v0_ratio(mp3: &[u8]) -> String {
    use packmp3::app::{analyze_frames, read_mp3};
    use packmp3::bitops::{IoSource, IoStream, MODE_READ, MODE_WRITE, TYPE_MEMORY};
    use packmp3::pmp::compress_mp3;
    use packmp3::state::PackState;

    let mut st = PackState::new();
    let mut src = IoStream::new(&IoSource::Memory(mp3), TYPE_MEMORY, 0, MODE_READ);
    assert!(read_mp3(&mut st, &mut src), "read_mp3 failed: {}", st.errormessage);
    assert!(analyze_frames(&mut st), "analyze_frames failed: {}", st.errormessage);
    let mut out = IoStream::new(&IoSource::Memory(&[]), TYPE_MEMORY, 0, MODE_WRITE);
    assert!(compress_mp3(&mut st, &mut out), "compress_mp3 failed: {}", st.errormessage);
    let pmp = out.getptr();
    let cr = 100.0 * pmp.len() as f64 / mp3.len() as f64;
    format!("{:.2}", cr)
}

#[test]
fn cli_golden_help_output() {
    // running without files shows the banner + help screen; the exact text is
    // locked down so any UI drift from the C++ original is caught
    let out = Command::new(bin()).output().expect("run packmp3");
    assert!(!out.status.success(), "help path must exit non-zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let golden = concat!(
        "\n--> packMP3 v1.0g (01/22/2016) by Matthias Stirner <--\n",
        "Copyright 2010-2016 Ratisbon University & Matthias Stirner\n",
        "All rights reserved\n",
        "\n",
        "\n",
        "Website: http://packjpg.encode.ru/\n",
        "Email  : packjpg (at) matthiasstirner.com\n",
        "\n",
        "Usage: packMP3 [switches] [filename(s)]\n",
        "\n",
        " [-ver]   verify files after processing\n",
        " [-v?]    set level of verbosity (max: 2) (def: 0)\n",
        " [-np]    no pause after processing files\n",
        " [-o]     overwrite existing files\n",
        " [-p]     proceed on warnings\n",
        "\n",
        "Examples: \"packMP3 -v1 -o luka.pmp\"\n",
        "          \"packMP3 -p *.mp3\"\n",
    );
    assert_eq!(stdout, golden);
}

#[test]
fn cli_golden_v0_compress_output() {
    // -v0 output is fully deterministic (no timing lines), so the whole screen
    // can be compared byte for byte against a golden built from the format
    // strings. Only the compression ratio is computed (same code path as the
    // binary, so the float formatting matches exactly).
    let (dir, _mp3_path, orig) = setup_huff("golden_v0", "song.mp3", 0x6E97_DCBA, 6);
    let ratio = expected_v0_ratio(&orig);
    let golden = format!(
        "\n--> packMP3 v1.0g (01/22/2016) by Matthias Stirner <--\n\
Copyright 2010-2016 Ratisbon University & Matthias Stirner\n\
All rights reserved\n\
\n\
\n\
Processing file 1 of 1 \"song.mp3\" -> Compressing -> {}%\n\
\n\
-> 1 file(s) processed, 0 error(s), 0 warning(s)\n",
        ratio
    );
    let (ok, stdout, stderr) = run(&dir, &["-np", "-v0", "-o", "song.mp3"]);
    assert!(ok, "compress failed: {}\n{}", stdout, stderr);
    assert_eq!(stdout, golden, "-v0 output differs from golden");
}

#[test]
fn cli_compresses_mp3_to_pmp() {
    let (dir, _mp3_path, _orig) = setup("compress", "song.mp3", 6);
    let (ok, stdout, stderr) = run(&dir, &["-np", "-o", "song.mp3"]);
    assert!(ok, "compress failed: {}\n{}", stdout, stderr);
    let pmp_path = dir.join("song.pmp");
    assert!(pmp_path.exists(), "no song.pmp produced");
    let pmp = fs::read(&pmp_path).unwrap();
    assert_eq!(&pmp[..2], b"MS", "pmp magic missing");
    // default verbosity is the progress bar; assert some progress-bar output
    assert!(
        stdout.contains("Processing file") || stderr.contains("Processing file"),
        "expected progress UI, got: {}",
        stdout
    );
}

#[test]
fn cli_decompresses_pmp_to_mp3() {
    let (dir, mp3_path, orig) = setup("decompress", "song.mp3", 6);
    // first compress
    let (ok, _, _) = run(&dir, &["-np", "-o", "song.mp3"]);
    assert!(ok);
    let pmp_path = dir.join("song.pmp");
    assert!(pmp_path.exists());
    // remove the original mp3 so decompression has a clean output name
    let mp3_out = dir.join("song.mp3");
    fs::remove_file(&mp3_path).unwrap();
    // decompress back
    let (ok, stdout, stderr) = run(&dir, &["-np", "-o", "song.pmp"]);
    assert!(ok, "decompress failed: {}\n{}", stdout, stderr);
    let back = fs::read(&mp3_out).unwrap();
    assert_eq!(back, orig, "decompressed MP3 differs from original");
}

#[test]
fn cli_verify_round_trip_ok() {
    let (dir, _mp3_path, _orig) = setup("verify", "song.mp3", 6);
    // -ver compresses, then decompresses in memory and compares
    let (ok, stdout, stderr) = run(&dir, &["-np", "-ver", "-o", "song.mp3"]);
    assert!(ok, "verify failed: {}\n{}", stdout, stderr);
    let pmp_path = dir.join("song.pmp");
    assert!(pmp_path.exists());
}

#[test]
fn cli_verify_huffman_round_trip_ok() {
    // the 'hard' synthetic files (real huffman-coded main data with linbits
    // escapes) must also survive the in-memory verify byte-compare
    let (dir, _mp3_path, _orig) = setup_huff("verify_huff", "song.mp3", 0xCAFE_F00D, 10);
    let (ok, stdout, stderr) = run(&dir, &["-np", "-ver", "-o", "song.mp3"]);
    assert!(ok, "verify failed: {}\n{}", stdout, stderr);
    assert!(dir.join("song.pmp").exists());
}

#[test]
fn cli_decompresses_huffman_mp3_byte_identical() {
    // compress + decompress the huffman file through the real binary and
    // compare the reconstructed MP3 byte-for-byte with the original
    let (dir, mp3_path, orig) = setup_huff("huff_rt", "song.mp3", 0x5EED_1234, 8);
    let (ok, _, _) = run(&dir, &["-np", "-o", "song.mp3"]);
    assert!(ok);
    assert!(dir.join("song.pmp").exists());
    fs::remove_file(&mp3_path).unwrap();
    let (ok, stdout, stderr) = run(&dir, &["-np", "-o", "song.pmp"]);
    assert!(ok, "decompress failed: {}\n{}", stdout, stderr);
    let back = fs::read(dir.join("song.mp3")).unwrap();
    assert_eq!(back, orig, "huffman MP3 round-trip is not byte-identical");
}

#[test]
fn cli_verify_detects_corruption() {
    let (dir, mp3_path, _orig) = setup("verifybad", "song.mp3", 6);
    // compress, then corrupt the pmp file in the middle of the payload
    let (ok, _, _) = run(&dir, &["-np", "-o", "song.mp3"]);
    assert!(ok);
    let pmp_path = dir.join("song.pmp");
    let pmp = fs::read(&pmp_path).unwrap();
    let idx = pmp.len() / 2;
    fs::write(&pmp_path, [&pmp[..idx], &[pmp[idx] ^ 0xFF], &pmp[idx + 1..]].concat()).unwrap();
    // decompress with -ver: with a corrupted payload the round-trip must not
    // silently succeed. The output mp3 is removed again on any error, so the
    // deterministic check is that no usable song.mp3 is left behind.
    fs::remove_file(&mp3_path).unwrap();
    run(&dir, &["-np", "-ver", "-o", "song.pmp"]);
    assert!(
        !dir.join("song.mp3").exists(),
        "corrupted pmp must not produce a valid output file"
    );
}

#[test]
fn cli_rejects_garbage_input() {
    let dir = std::env::temp_dir().join("packmp3_cli_garbage");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let garbage = dir.join("junk.mp3");
    fs::write(&garbage, b"this is definitely not an mp3").unwrap();
    let (ok, stdout, stderr) = run(&dir, &["-np", "junk.mp3"]);
    // packmp3 reports errors per file but still exits 0 (C++ main returns 0)
    assert!(ok, "process should exit 0 with the error reported in the summary");
    let all = format!("{}\n{}", stdout, stderr);
    assert!(
        all.contains("no mpeg audio data recognized"),
        "expected error message, got: {}",
        all
    );
    // no output file may be left behind
    assert!(!dir.join("junk.pmp").exists());
}

#[test]
fn cli_missing_file_reports_read_error() {
    let dir = std::env::temp_dir().join("packmp3_cli_missing");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let (ok, stdout, stderr) = run(&dir, &["-np", "nope.mp3"]);
    assert!(ok, "process should exit 0 with the error reported in the summary");
    let all = format!("{}\n{}", stdout, stderr);
    assert!(
        all.contains("could not read file / file not found"),
        "expected read error, got: {}",
        all
    );
}

#[test]
fn cli_show_help_without_files() {
    // no files at all -> help screen, non-zero exit
    let out = Command::new(bin()).output().expect("run packmp3");
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage: packMP3 [switches] [filename(s)]"));
    assert!(stdout.contains("Website: http://packjpg.encode.ru/"));
}

#[test]
fn cli_verbosity_levels() {
    let (dir, _mp3_path, _orig) = setup("verbosity", "song.mp3", 6);
    // -v1: standard UI with DONE/ERROR line
    let (ok, stdout, _) = run(&dir, &["-np", "-v1", "-o", "song.mp3"]);
    assert!(ok);
    assert!(stdout.contains("DONE"), "expected DONE, got: {}", stdout);
    assert!(stdout.contains("Processing file 1 of 1 \"song.mp3\" -> Compressing -> DONE"));
    assert!(stdout.contains(" time taken  : "), "expected timing info");
    assert!(stdout.contains(" comp. ratio : "), "expected ratio info");

    // -v0: only the ratio, no statistics block
    let (dir2, _mp3_path2, _orig2) = setup("verbosity0", "song.mp3", 6);
    let (ok, stdout, _) = run(&dir2, &["-np", "-v0", "-o", "song.mp3"]);
    assert!(ok);
    assert!(
        stdout.contains("%"),
        "expected percentage output, got: {}",
        stdout
    );
    // the file count summary is always printed, but the statistics block is
    // only shown for verbosity != 0
    assert!(
        stdout.contains("file(s) processed"),
        "expected file summary, got: {}",
        stdout
    );
    assert!(
        !stdout.contains(" total time        : "),
        "v0 should not print the statistics block, got: {}",
        stdout
    );
}

#[test]
fn cli_verbosity2_shows_status_lines() {
    let (dir, _mp3_path, _orig) = setup("verbosity2", "song.mp3", 6);
    let (ok, stdout, _) = run(&dir, &["-np", "-v2", "-o", "song.mp3"]);
    assert!(ok);
    assert!(stdout.contains("Reading MP3"), "expected status, got: {}", stdout);
    assert!(stdout.contains("Analysing frames"));
    assert!(stdout.contains("Compressing to PMP"));
    assert!(stdout.contains("-> Compressing OK"));
    // C++ fprintf("\n---...") has no trailing newline, so the opening dashes
    // must be immediately followed by the first status line (no blank line)
    assert!(
        stdout.contains("----------------------------------------\nDetermining filetype"),
        "opening dashes must be immediately followed by the first status line, got: {}",
        stdout
    );
    // same for the closing dashes followed directly by the OK line
    assert!(
        stdout.contains("----------------------------------------\n-> Compressing OK"),
        "closing dashes must be immediately followed by the OK line, got: {}",
        stdout
    );
}

#[test]
fn cli_formatting_ratios_use_single_percent() {
    // C++ printf("%7.2f %%") prints a single "%" (printf %% escaping).
    // The Rust port must not emit the literal "%%".
    for v in ["-v1", "-v2"] {
        let (dir, _mp3_path, _orig) = setup(&format!("percent{}", &v[1..]), "song.mp3", 6);
        let (ok, stdout, _) = run(&dir, &["-np", v, "-o", "song.mp3"]);
        assert!(ok);
        assert!(
            !stdout.contains("%%"),
            "{}: ratio lines must print a single % (C++ printf %% escaping), got: {}",
            v,
            stdout
        );
        let ratio_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.contains("comp. ratio"))
            .collect();
        assert!(!ratio_lines.is_empty(), "{}: expected ratio lines", v);
        for l in &ratio_lines {
            assert!(
                l.trim_end().ends_with('%'),
                "{}: ratio line must end in a single %: {:?}",
                v,
                l
            );
        }
    }
}

#[test]
fn cli_unique_filename_when_output_exists() {
    // without -o, an existing output is not overwritten but gets an underscore
    let (dir, _mp3_path, _orig) = setup("unique", "song.mp3", 6);
    let (ok, _, _) = run(&dir, &["-np", "song.mp3"]);
    assert!(ok);
    let first = dir.join("song.pmp");
    assert!(first.exists());
    // run again without -o: should create song_.pmp instead of overwriting
    let (ok, _, _) = run(&dir, &["-np", "song.mp3"]);
    assert!(ok);
    assert!(dir.join("song_.pmp").exists(), "expected song_.pmp");
    // and the original song.pmp must be untouched
    assert!(first.exists());
}
