# packmp3 (Rust)

A clean-room **Rust port** of the [packMP3](https://www.mpack.cn/) MP3 further-compression
tool by Matthias Stirner (Ratisbon University, © 2010–2016). It losslessly compresses
MPEG-1 Audio Layer III (MP3) files into the **PMP** format and decompresses them back,
with a command-line interface that mirrors the original C++ program.

This port is written in **100% safe Rust with zero external dependencies**, and its
output is **byte-identical** to the reference C++ binary:

- `.pmp` compression output is byte-for-byte identical;
- decompressed `.mp3` files round-trip losslessly (bit-exact);
- the console UI matches the C++ output byte-for-byte (except wall-clock timing values).

## Features

- **Lossless**: PMP → MP3 decompression restores the original file bit-exactly.
- **Byte-identical to the C++ reference**: verified against `packMP3.exe` on real MP3s
  (PMP hash, round-trip hash and console output).
- **No external dependencies**: pure standard-library Rust (edition 2021).
- **Full CLI parity** with the original tool, including verbosity levels, overwrite
  policy, verify mode, progress bar, and `< press ENTER >` pause behaviour.
- **Batch processing** of multiple files, wildcards and `STDIN` piping.
- Real MPEG-1 Layer III **Huffman decoding** of the main data (mono and stereo,
  40–320 kbps) — not just silent test frames.

## Building

Requires a recent stable Rust toolchain (1.70+).

```sh
cargo build --release
```

The binary is produced at `target/release/packmp3[.exe]`.

## Usage

```
packmp3 [switches] [filename(s)]
```

### Options

| Switch     | Description                                    |
|------------|------------------------------------------------|
| `-ver`     | verify files after processing                  |
| `-v?`      | set level of verbosity (max: 2) (def: 0)       |
| `-np`      | no pause after processing files                |
| `-o`       | overwrite existing files                       |
| `-p`       | proceed on warnings                            |

Verbosity levels:

- `-v0` — minimal output (a `NN.NN%` compression ratio or `DONE`);
- `-v1` — standard UI with a summary block;
- `-v2` — detailed UI showing each processing step and its timing;
- `-vp` — progress-bar UI (default when `-v` is not given).

Without `-o`, an existing output file is **not** overwritten — a uniquely numbered
output name is used instead.

### Examples

Compress an MP3 file (writes `luka.pmp`):

```sh
packmp3 -v1 -o luka.mp3
```

Decompress a PMP file back to MP3 (passing a `.pmp` file switches to decompression):

```sh
packmp3 -v2 -o luka.pmp
```

Process several files, proceeding on warnings:

```sh
packmp3 -p *.mp3
```

Compress and then verify the round trip:

```sh
packmp3 -np -v2 -ver -o music.mp3
```

## How it works

MP3 frames store their main data as **Huffman-coded** spectral values chosen from a set
of codebooks. The remaining redundant bits (side information, scale factors and unused
Huffman-code padding) are still a significant fraction of the stream. packMP3 parses
each MP3 frame, strips this redundancy and repacks the essential data into a compact
PMP stream. The decompressor rebuilds a byte-identical MP3 file from the PMP data.

## Testing

The test suite exercises both synthetic and real Huffman-coded MP3 frames, including
CLI round-trips, `-ver` verification, and golden output checks against the C++
reference formatting:

```sh
cargo test
cargo clippy --all-targets
```

The `examples/genmp3.rs` helper generates silent or Huffman-coded MP3 test files:

```sh
cargo run --release --example genmp3 -- out.mp3 [nframes] [mono] [huffman] [bitrate]
```

`mono` accepts `1`/`mono`, `huffman` accepts `1`/`huff`/`huffman`, and `bitrate` is the
MPEG-1 Layer III bitrate-table index `1..14` (default 9 = 128 kbps). For example, a
stereo Huffman-coded file at 320 kbps:

```sh
cargo run --release --example genmp3 -- out.mp3 6 0 huff 14
```

## Project layout

```
src/
  cli.rs         command-line UI (port of the C++ CLI)
  app.rs         application-level processing (read/analyze/compress/decompress)
  mp3.rs         MP3 frame parsing (header, side info, Huffman main data)
  huffmp3.rs     Huffman decoding of the main data
  hufftables.rs  Huffman code tables
  pmp.rs         PMP container encode/decode
  aricoder.rs    arithmetic coder used by the PMP stream
  bitops.rs      bit-level I/O streams (file/memory)
  tables.rs      MPEG-1 bitrate/samplerate tables
  state.rs       shared processing state
  testsupport.rs test-only MP3 builder utilities
tests/           integration tests (round-trip, CLI, golden output)
examples/        genmp3.rs — MP3 test-file generator
tools/           gen_tables.py — regenerates the static tables
```

## Port fidelity

This project is an independent reimplementation of the published packMP3 behaviour and
file format. It produces **byte-identical** results to the original C++ binary
(`packMP3 v1.0g`), which was verified in a differential test:

- `music.mp3` (7.4 MB) compressed by both binaries → identical 6,074,982-byte `.pmp`
  (SHA-256 `E551C9D6D6E12F3963C94D85B787F88CB008E1C8D1CF2D54CF41D7184983B0B7`);
- decompressing both `.pmp` files → MP3s bit-identical to the source `music.mp3`;
- `-v2` / `-v1` / `-ver` console output identical except for wall-clock timing values.

## License

This project is licensed under the **GNU Lesser General Public License v3.0 or later**
— see [LICENSE](LICENSE). The PMP format and the original packMP3 tool are by Matthias
Stirner / Ratisbon University.
