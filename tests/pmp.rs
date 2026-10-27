// pmp.rs - integration tests for the PMP codec: MP3 -> PMP -> MP3 round-trip.
//
// A tiny but *valid* MPEG-1 Layer III file is synthesized first (silent frames,
// zero coefficients, no scalefactors, no bit reservoir) and then pushed through
// the whole application pipeline: read_mp3 -> analyze_frames -> compress_mp3
// -> uncompress_pmp -> write_mp3. The reconstructed MP3 must be byte-identical
// to the original.

mod common;

use common::build_silent_mp3;
use packmp3::app::{analyze_frames, read_mp3, reset_buffers, write_mp3};
use packmp3::hufftables::BV_ENC_TABLE;
use packmp3::pmp::{compress_mp3, uncompress_pmp};
use packmp3::state::PackState;
use packmp3::testsupport::build_huffman_mp3;
use packmp3::bitops::{IoSource, IoStream, MODE_READ, MODE_WRITE, TYPE_MEMORY};

/// run the full round trip, returning (compressed, decompressed) byte vectors
fn roundtrip(original: &[u8]) -> (Vec<u8>, Vec<u8>) {
    // --- compress: MP3 -> PMP ---
    let mut st = PackState::new();
    let mut src = IoStream::new(&IoSource::Memory(original), TYPE_MEMORY, 0, MODE_READ);
    assert!(
        read_mp3(&mut st, &mut src),
        "read_mp3 failed: {}",
        st.errormessage
    );
    assert!(
        analyze_frames(&mut st),
        "analyze_frames failed: {}",
        st.errormessage
    );
    let mut pmp_out = IoStream::new(&IoSource::Memory(&[]), TYPE_MEMORY, 0, MODE_WRITE);
    assert!(
        compress_mp3(&mut st, &mut pmp_out),
        "compress_mp3 failed: {}",
        st.errormessage
    );
    let pmp_bytes = pmp_out.getptr();

    // --- decompress: PMP -> MP3 ---
    let mut st2 = PackState::new();
    let mut pmp_in = IoStream::new(&IoSource::Memory(&pmp_bytes), TYPE_MEMORY, 0, MODE_READ);
    assert!(
        uncompress_pmp(&mut st2, &mut pmp_in),
        "uncompress_pmp failed: {}",
        st2.errormessage
    );
    let mut mp3_out = IoStream::new(&IoSource::Memory(&[]), TYPE_MEMORY, 0, MODE_WRITE);
    assert!(
        write_mp3(&mut st2, &mut mp3_out),
        "write_mp3 failed: {}",
        st2.errormessage
    );
    let mp3_bytes = mp3_out.getptr();

    (pmp_bytes, mp3_bytes)
}

#[test]
fn roundtrip_silent_stereo_is_byte_identical() {
    let original = build_silent_mp3(6, false);
    let (pmp, mp3) = roundtrip(&original);
    assert_eq!(mp3, original);
    // the PMP file must actually compress (smaller) and start with the magic
    assert!(pmp.len() < original.len());
    assert_eq!(&pmp[..2], b"MS");
}

#[test]
fn roundtrip_silent_mono_is_byte_identical() {
    // mono exercises the 17-byte side info and single-channel code paths
    let original = build_silent_mp3(6, true);
    let (pmp, mp3) = roundtrip(&original);
    assert_eq!(mp3, original);
    assert!(pmp.len() < original.len());
}

#[test]
fn roundtrip_more_frames_is_byte_identical() {
    // a longer file stresses the bit reservoir / aux prediction logic
    let original = build_silent_mp3(50, false);
    let (_pmp, mp3) = roundtrip(&original);
    assert_eq!(mp3, original);
}

#[test]
fn roundtrip_compressed_smaller_than_original() {
    let original = build_silent_mp3(20, false);
    let (pmp, _mp3) = roundtrip(&original);
    assert!(pmp.len() < original.len() / 2);
}

#[test]
fn read_mp3_rejects_garbage() {
    let mut st = PackState::new();
    let garbage = b"this is not an mp3 file at all........";
    let mut src = IoStream::new(&IoSource::Memory(garbage), TYPE_MEMORY, 0, MODE_READ);
    assert!(!read_mp3(&mut st, &mut src));
    assert_eq!(st.errormessage, "no mpeg audio data recognized");
}

#[test]
fn read_mp3_rejects_empty_input() {
    let mut st = PackState::new();
    let mut src = IoStream::new(&IoSource::Memory(&[]), TYPE_MEMORY, 0, MODE_READ);
    assert!(!read_mp3(&mut st, &mut src));
    assert_eq!(st.errormessage, "out of memory error");
}

#[test]
fn reset_buffers_clears_state() {
    let mut st = PackState::new();
    st.g_nframes = 7;
    st.main_data_size = 123;
    st.firstframe = Some(0);
    reset_buffers(&mut st);
    assert_eq!(st.g_nframes, 0);
    assert_eq!(st.main_data_size, 0);
    assert!(st.firstframe.is_none());
    assert!(st.frames.is_empty());
    assert!(st.granules.is_empty());
    assert_eq!(st.ai.i_samplerate, -1);
    assert_eq!(st.ai.i_channels, -1);
}

#[test]
fn analyze_frames_computes_globals() {
    let original = build_silent_mp3(8, false);
    let mut st = PackState::new();
    let mut src = IoStream::new(&IoSource::Memory(&original), TYPE_MEMORY, 0, MODE_READ);
    assert!(read_mp3(&mut st, &mut src));
    assert!(analyze_frames(&mut st));
    assert_eq!(st.g_nframes, 8);
    assert_eq!(st.g_nchannels, 2); // stereo
    assert_eq!(st.g_samplerate, 44100);
    assert_eq!(st.g_bitrate, 128);
    // all frames are consistent, so nothing is marked "variable"
    assert_eq!(st.ai.i_bitrate, 9);
    assert_eq!(st.ai.i_padding, 0);
    assert_eq!(st.ai.i_bit_res, 0);
    assert_eq!(st.ai.i_stereo_ms, 0);
}

/// sanity check for the synthetic huffman files: they must really contain
/// huffman-coded main data, otherwise the round-trip tests below are vacuous
fn assert_has_huffman_data(mp3: &[u8]) {
    let mut st = PackState::new();
    let mut src = IoStream::new(&IoSource::Memory(mp3), TYPE_MEMORY, 0, MODE_READ);
    assert!(
        read_mp3(&mut st, &mut src),
        "read_mp3 failed: {}",
        st.errormessage
    );
    // beside huffman-coded bits, we also expect escapes (symbol 15 + linbits)
    assert!(st.n_bad_first == 0, "synthetic file was flagged as broken");
    let mut any_big = false;
    let mut any_escape = false;
    for f in &st.frames {
        for gi in f.granules.iter().flatten() {
            let g = &st.granules[*gi];
            if g.big_val_pairs > 0 && g.main_data_bit > 0 {
                any_big = true;
            }
            if g.region_table.iter().any(|&rt| rt != 0 && BV_ENC_TABLE[rt as usize].linbits > 0) {
                any_escape = true;
            }
        }
    }
    assert!(any_big, "no huffman-coded big-value data found in synthetic file");
    assert!(
        any_escape,
        "no region with linbits tables found in synthetic file"
    );
}

#[test]
fn huffman_mp3_builder_produces_real_huffman_data() {
    // guard: the builder must produce non-trivial main data
    let mp3 = build_huffman_mp3(0x1234_5678, 8, false, 9);
    // and it must differ from the all-silent baseline in its main data
    let silent = build_silent_mp3(8, false);
    assert_ne!(mp3, silent);
    assert_has_huffman_data(&mp3);

    let mut st = PackState::new();
    let mut src = IoStream::new(&IoSource::Memory(&mp3), TYPE_MEMORY, 0, MODE_READ);
    assert!(read_mp3(&mut st, &mut src));
    assert!(analyze_frames(&mut st));
    assert!(st.g_nframes == 8);
    // with varied scalefactor/coefficient bit usage, main data should vary
    let sizes: Vec<i32> = st.frames.iter().map(|f| f.main_size).collect();
    assert!(sizes.windows(2).any(|w| w[0] != w[1]), "main sizes should vary");
}

#[test]
fn roundtrip_huffman_main_data_stereo_is_byte_identical() {
    let original = build_huffman_mp3(0x1234_5678, 8, false, 9);
    let (pmp, mp3) = roundtrip(&original);
    assert_eq!(mp3, original);
    // the huffman-coded file must still compress
    assert!(pmp.len() < original.len());
    assert_eq!(&pmp[..2], b"MS");
}

#[test]
fn roundtrip_huffman_main_data_mono_is_byte_identical() {
    // mono exercises the j_coding=false path with the same huffman data
    let original = build_huffman_mp3(0xDEAD_BEEF, 8, true, 9);
    let (pmp, mp3) = roundtrip(&original);
    assert_eq!(mp3, original);
    assert!(pmp.len() < original.len());
}

#[test]
fn roundtrip_huffman_main_data_many_frames_is_byte_identical() {
    // a longer file stresses bit-reservoir/aux handling across more frames
    let original = build_huffman_mp3(0x0DDB1A5E, 30, false, 9);
    let (_pmp, mp3) = roundtrip(&original);
    assert_eq!(mp3, original);
}

#[test]
fn roundtrip_huffman_main_data_various_bitrates_is_byte_identical() {
    // exercise different CBR frame sizes and aux budgets: 40 kbps (idx 2),
    // 64 kbps (idx 5), 128 kbps (idx 9), 192 kbps (idx 11), 320 kbps (idx 14)
    for bitrate_idx in [2i32, 5, 9, 11, 14] {
        let original = build_huffman_mp3(0xB17 + bitrate_idx as u32, 10, false, bitrate_idx);
        let (_pmp, mp3) = roundtrip(&original);
        assert_eq!(mp3, original, "round-trip failed at bitrate idx {}", bitrate_idx);
    }
}
