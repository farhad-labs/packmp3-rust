// mp3.rs unit tests
//
// The C++ original had no test suite; these tests validate the Rust port's
// frame parsing/building, CRC, tag extraction and seek behaviour using
// hand-crafted MPEG-1 Layer III frame bytes. Expected CRC values were computed
// independently (Python) from the exact crc_table[] data in source/pmp3tbl.h.

use packmp3::huffmp3::HuffmanReader;
use packmp3::mp3::{
    mp3_append_frame, mp3_build_fixed, mp3_calc_layer3_crc, mp3_decode_frame, mp3_get_id3_size,
    mp3_mute_frame, mp3_read_frame, mp3_seek_firstframe, mp3_unmute_frame,
};
use packmp3::state::{PackState, EL_WARNING};

/// craft a minimal, valid stereo MPEG-1 Layer III frame (128 kbps, 44.1 kHz,
/// no CRC) with an all-zero side info (everything muted). Returns 417 bytes.
fn stereo_mute_frame() -> Vec<u8> {
    let mut f = Vec::with_capacity(417);
    // header: FF FB 90 40
    f.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    f.resize(4 + 32, 0); // 32 bytes of zero side info
    f.resize(417, 0); // main data (none) + aux/stuffing
    f
}

#[test]
fn read_frame_parses_valid_stereo_header() {
    let mut st = PackState::new();
    let data = stereo_mute_frame();
    let fidx = mp3_read_frame(&mut st, &data, data.len() as i32).expect("frame should parse");
    let fr = &st.frames[fidx];
    assert_eq!(fr.frame_size, 417);
    assert_eq!(fr.fixed_size, 36); // 4 + 32, no CRC
    assert_eq!(fr.nchannels, 2);
    assert_eq!(fr.mpeg, 3);
    assert_eq!(fr.layer, 1);
    assert_eq!(fr.protection, 0);
    assert_eq!(fr.bits, 9);
    assert_eq!(fr.samples, 0);
    assert_eq!(fr.channels, 0); // stereo
    assert_eq!(fr.main_size, 0);
    assert_eq!(fr.main_bits, 0);
    assert_eq!(fr.aux_size, 417 - 36); // 381
    assert_eq!(fr.bit_reservoir, 0);
    // all granules should have default values
    for ch in 0..2 {
        for gr in 0..2 {
            let g = fr.granule(&st, ch, gr);
            assert_eq!(g.main_data_bit, 0);
            assert_eq!(g.big_val_pairs, 0);
            assert_eq!(g.window_switching, 0);
            assert_eq!(g.block_type, 0);
        }
    }
}

#[test]
fn read_frame_rejects_garbage() {
    let mut st = PackState::new();
    // no syncword
    assert!(mp3_read_frame(&mut st, &[0x00; 64], 64).is_none());
    // wrong mpeg version (not MPEG-1 LAYER III)
    let mut d = vec![0u8; 64];
    d[0] = 0xFF;
    d[1] = 0xFA; // MPEG-1 L3 ok
    d[1] = 0xF0; // version bits = 00 -> MPEG-2.5, rejected by &0xFA
    assert!(mp3_read_frame(&mut st, &d, 64).is_none());
    // too short
    assert!(mp3_read_frame(&mut st, &[0xFF, 0xFA, 0x90], 3).is_none());
    // bad bitrate / samplerate index
    let mut d = stereo_mute_frame();
    d[2] = 0x00; // bits = 0x0 (free form) -> rejected
    assert!(mp3_read_frame(&mut st, &d, d.len() as i32).is_none());
    let mut d = stereo_mute_frame();
    d[2] = 0xF0; // bits = 0xF (invalid)
    assert!(mp3_read_frame(&mut st, &d, d.len() as i32).is_none());
    let mut d = stereo_mute_frame();
    d[2] = 0x0C; // samples = 0x3 (reserved)
    assert!(mp3_read_frame(&mut st, &d, d.len() as i32).is_none());
}

#[test]
fn build_fixed_roundtrip_stereo_no_crc() {
    let mut st = PackState::new();
    let data = stereo_mute_frame();
    let fidx = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    let fixed = mp3_build_fixed(&mut st, fidx);
    // build_fixed returns the fixed part (header + side info) = 36 bytes
    assert_eq!(fixed, &data[0..36]);
}

#[test]
fn build_fixed_roundtrip_with_crc() {
    let mut st = PackState::new();
    let mut data = stereo_mute_frame();
    // turn on CRC: protection bit in header = 0
    data[1] = 0xFA;
    let fidx = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    assert_eq!(st.frames[fidx].protection, 1);
    let fixed = mp3_build_fixed(&mut st, fidx);
    assert_eq!(fixed[0..4], [0xFF, 0xFA, 0x90, 0x00]);
    assert_eq!(fixed.len(), 4 + 2 + 32);
    // check that the CRC written is the value read back by the parser
    let crc = ((fixed[4] as u16) << 8) | fixed[5] as u16;
    let expect = mp3_calc_layer3_crc(&[0xFF, 0xFA, 0x90, 0x00], &[0u8; 32], 32);
    assert_eq!(crc, expect);
}

#[test]
fn crc_matches_independently_computed_values() {
    // values computed in Python from the exact crc_table[] in source/pmp3tbl.h
    let header = [0xFF, 0xFA, 0x90, 0x40];
    assert_eq!(
        mp3_calc_layer3_crc(&header, &[0u8; 32], 32),
        0x5B5F
    );
    assert_eq!(
        mp3_calc_layer3_crc(&header, &[0xABu8; 32], 32),
        0x0739
    );
    assert_eq!(
        mp3_calc_layer3_crc(&header, &(0..32).map(|i| i as u8).collect::<Vec<u8>>(), 32),
        0xFEEB
    );
    // mono: header with bitrate idx 8, samplerate 0, mono; 17 bytes sideinfo
    let hdr2 = [0xFF, 0xF9, 0x90, 0x00];
    assert_eq!(
        mp3_calc_layer3_crc(&hdr2, &[0x11u8; 32], 32),
        0x59C0
    );
    let hdr3 = [0xFF, 0xFA, 0x80, 0x00];
    assert_eq!(
        mp3_calc_layer3_crc(&hdr3, &[0x22u8; 17], 17),
        0x0329
    );
}

#[test]
fn crc_mismatch_sets_warning() {
    let mut st = PackState::new();
    let mut data = stereo_mute_frame();
    data[1] = 0xFA; // CRC present
    data[4] = 0x00; // wrong CRC bytes
    data[5] = 0x00;
    let fidx = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    assert_eq!(st.errorlevel, EL_WARNING);
    assert_eq!(st.errormessage, "crc checksum mismatch");
    let _ = fidx;
}

#[test]
fn append_frame_links_chain_and_granules() {
    let mut st = PackState::new();
    let data = stereo_mute_frame();
    let f0 = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    let f1 = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    mp3_append_frame(&mut st, f0);
    mp3_append_frame(&mut st, f1);
    assert_eq!(st.firstframe, Some(f0));
    assert_eq!(st.lastframe, Some(f1));
    assert_eq!(st.frames[f0].n, 0);
    assert_eq!(st.frames[f1].n, 1);
    assert_eq!(st.frames[f1].prev, Some(f0));
    assert_eq!(st.frames[f0].next, Some(f1));
    // granule numbers + links per channel
    for ch in 0..2 {
        let g0a = st.frames[f0].granules[ch][0];
        let g1a = st.frames[f0].granules[ch][1];
        let g0b = st.frames[f1].granules[ch][0];
        let g1b = st.frames[f1].granules[ch][1];
        assert_eq!(st.granules[g0a].n, 0);
        assert_eq!(st.granules[g1a].n, 1);
        assert_eq!(st.granules[g0b].n, 2);
        assert_eq!(st.granules[g1b].n, 3);
        assert_eq!(st.granules[g0a].next, Some(g1a));
        assert_eq!(st.granules[g1a].prev, Some(g0a));
        assert_eq!(st.granules[g1a].next, Some(g0b));
        assert_eq!(st.granules[g0b].prev, Some(g1a));
        assert_eq!(st.granules[g1b].next, None);
    }
}

#[test]
fn seek_firstframe_finds_frames_after_garbage() {
    // craft 5 consecutive valid frames at a known offset
    let frame = stereo_mute_frame(); // 417 bytes each
    let mut data: Vec<u8> = Vec::new();
    data.extend_from_slice(&[0x00, 0x01, 0x02, 0x03]); // garbage
    for _ in 0..5 {
        data.extend_from_slice(&frame);
    }
    let pos = mp3_seek_firstframe(&data, data.len() as i32);
    assert_eq!(pos, 4);
}

#[test]
fn seek_firstframe_returns_negative_for_garbage_only() {
    let data = vec![0x42u8; 4096];
    assert_eq!(mp3_seek_firstframe(&data, data.len() as i32), -1);
}

#[test]
fn get_id3_size_v1_v2_and_none() {
    // no tag
    let none = vec![0u8; 64];
    assert_eq!(mp3_get_id3_size(&none, 64), 0);
    // ID3v1 (128 bytes)
    let mut v1 = vec![0u8; 128];
    v1[0..3].copy_from_slice(b"TAG");
    assert_eq!(mp3_get_id3_size(&v1, 128), 128);
    // too small buffer -> 0
    assert_eq!(mp3_get_id3_size(&v1, 4), 0);
    // ID3v2: 10-byte header + syncsafe size (0) + footer flag off
    let mut v2 = vec![0u8; 64];
    v2[0..3].copy_from_slice(b"ID3");
    // bytes 6..9 are the 7-bit big-endian size (all 0 here)
    assert_eq!(mp3_get_id3_size(&v2, 64), 10);
    // ID3v2 with size
    let mut v2 = vec![0u8; 200];
    v2[0..3].copy_from_slice(b"ID3");
    v2[9] = 42; // size low byte
    assert_eq!(mp3_get_id3_size(&v2, 200), 10 + 42);
    // ID3v2 with footer flag
    let mut v2 = vec![0u8; 200];
    v2[0..3].copy_from_slice(b"ID3");
    v2[5] = 0x10; // footer flag
    v2[9] = 42;
    assert_eq!(mp3_get_id3_size(&v2, 200), 10 + 10 + 42);
    // unsynchronization: flag set, 0xFF 0x00 pair in the tag data -> +1
    let mut v2 = vec![0u8; 200];
    v2[0..3].copy_from_slice(b"ID3");
    v2[5] = 0x80; // unsync flag
    v2[9] = 42;
    v2[10] = 0xFF;
    v2[11] = 0x00;
    assert_eq!(mp3_get_id3_size(&v2, 200), 10 + 42 + 1);
}

#[test]
fn read_frame_reads_granule_side_info() {
    // craft side info manually with known values and check they're parsed back
    // bit pattern (stereo, 32 bytes = 256 bits):
    //   bit_reservoir 9 | padbits 3 | share(ch0) 4 | share(ch1) 4
    //   per granule: mdb 12 | bvp 9 | gg 8 | sl 4 | ws 1 |
    //     (normal) rt0 5 rt1 5 rt2 5 | r0s 4 | r1s 3 | pre 1 | coarse 1 | selb 1
    let mut side = vec![0u8; 32];
    // use a small helper: write bit-by-bit
    fn put(side: &mut [u8], bitpos: usize, bits: u32, n: usize) -> usize {
        let mut p = bitpos;
        for i in (0..n).rev() {
            let b = ((bits >> i) & 1) as u8;
            side[p / 8] |= b << (7 - (p % 8));
            p += 1;
        }
        p
    }
    let mut bp = 0;
    bp = put(&mut side, bp, 0b000010100, 9); // bit_reservoir = 20
    bp = put(&mut side, bp, 0b101, 3); // padbits = 5
    bp = put(&mut side, bp, 0b0011, 4); // share ch0 = 3
    bp = put(&mut side, bp, 0b0000, 4); // share ch1 = 0
    for gr in 0..2 {
        for ch in 0..2 {
            let mdb: u32 = if ch == 0 && gr == 0 { 100 } else { 0 };
            let bvp: u32 = if ch == 0 && gr == 0 { 7 } else { 0 };
            bp = put(&mut side, bp, mdb, 12);
            bp = put(&mut side, bp, bvp, 9);
            bp = put(&mut side, bp, 5, 8); // global_gain = 5
            bp = put(&mut side, bp, 2, 4); // slength = 2
            bp = put(&mut side, bp, 0, 1); // window_switching = 0
            bp = put(&mut side, bp, 3, 5); // region_table[0]
            bp = put(&mut side, bp, 2, 5); // region_table[1]
            bp = put(&mut side, bp, 1, 5); // region_table[2]
            bp = put(&mut side, bp, 4, 4); // region0_size
            bp = put(&mut side, bp, 1, 3); // region1_size
            bp = put(&mut side, bp, 1, 1); // preemphasis
            bp = put(&mut side, bp, 0, 1); // coarse
            bp = put(&mut side, bp, 1, 1); // select_htabB
        }
    }
    assert_eq!(bp, 256);

    let mut data = vec![0u8; 417];
    data[0..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x40]);
    data[4..4 + 32].copy_from_slice(&side);

    let mut st = PackState::new();
    let fidx = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    let fr = &st.frames[fidx];
    assert_eq!(fr.bit_reservoir, 20);
    assert_eq!(fr.padbits, 5);
    assert_eq!(fr.granule(&st, 0, 0).share, 3);
    let g = fr.granule(&st, 0, 0);
    assert_eq!(g.main_data_bit, 100);
    assert_eq!(g.big_val_pairs, 7);
    assert_eq!(g.global_gain, 5);
    assert_eq!(g.slength, 2);
    assert_eq!(g.region_table, [3, 2, 1]);
    assert_eq!(g.region0_size, 4);
    assert_eq!(g.region1_size, 1);
    assert_eq!(g.preemphasis, 1);
    assert_eq!(g.select_htabb, 1);
    // region bounds: bounds[5] = 20 but clamped to big_val_pairs<<1 = 14
    assert_eq!(g.region_bound[0], 14);
    assert_eq!(g.region_bound[1], 14);
    assert_eq!(g.region_bound[2], 14);
    // main data = 100 bits -> 13 bytes (rounded up)
    assert_eq!(fr.main_bits, 100);
    assert_eq!(fr.main_size, 13);
}

#[test]
fn append_frame_resets_counter_on_empty_chain() {
    let mut st = PackState::new();
    let data = stereo_mute_frame();
    // append one, then reset buffers, then append again -> n restarts at 0
    let f0 = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    mp3_append_frame(&mut st, f0);
    assert_eq!(st.frames[f0].n, 0);
    // simulate reset_buffers
    st.frames.clear();
    st.granules.clear();
    st.firstframe = None;
    st.lastframe = None;
    let f1 = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    mp3_append_frame(&mut st, f1);
    assert_eq!(st.frames[f1].n, 0);
}

#[test]
fn mute_unmute_roundtrip_restores_frame() {
    let mut st = PackState::new();
    let data = stereo_mute_frame();
    let fidx = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    // put some recognizable values in (flags kept clear so the bit-reservoir
    // reconstruction is exact; see the note below about the C++ quirk)
    {
        let fr = &mut st.frames[fidx];
        fr.privbit = 0;
        fr.original = 0;
        fr.copyright = 0;
        fr.bit_reservoir = 200; // < 256 so the top bit of the 9-bit field is 0
        for ch in 0..2 {
            for gr in 0..2 {
                let gidx = fr.granules[ch][gr];
                let g = &mut st.granules[gidx];
                g.main_data_bit = if ch == 0 && gr == 1 { 1500 } else { 3 };
                g.slength = 9;
                g.big_val_pairs = 250;
            }
        }
    }
    mp3_mute_frame(&mut st, fidx);
    // frame is now muted
    {
        let fr = &st.frames[fidx];
        assert_eq!(fr.bit_reservoir, 0);
        assert_eq!(fr.main_size, 0);
        assert_eq!(fr.main_bits, 0);
        for ch in 0..2 {
            for gr in 0..2 {
                let g = fr.granule(&st, ch, gr);
                assert_eq!(g.main_data_bit, 0);
                assert_eq!(g.big_val_pairs, 0);
                assert_eq!(g.slength, 0);
            }
        }
    }
    // unmute restores everything (the C++ function uses the global g_nchannels)
    st.g_nchannels = 2;
    st.n_bad_first = 0;
    assert!(mp3_unmute_frame(&mut st, fidx));
    let fr = &st.frames[fidx];
    assert_eq!(fr.privbit, 0);
    assert_eq!(fr.original, 0);
    assert_eq!(fr.copyright, 0);
    assert_eq!(fr.bit_reservoir, 200);
    for ch in 0..2 {
        for gr in 0..2 {
            let g = fr.granule(&st, ch, gr);
            let want_mdb = if ch == 0 && gr == 1 { 1500 } else { 3 };
            assert_eq!(g.main_data_bit, want_mdb);
            assert_eq!(g.slength, 9);
            assert_eq!(g.big_val_pairs, 250);
        }
    }
}

#[test]
fn unmute_bit_reservoir_leak_matches_cpp_quirk() {
    // C++ `mp3_unmute_frame` does `frame->bit_reservoir = (*ptr++) << 8;` where
    // *ptr is the whole first byte, which also holds the privbit/original/
    // copyright flags. Those flag bits land in the top bits of the short
    // reservoir. This is a faithful quirk of the original (not a bug in the
    // port), reproduced here deliberately.
    let mut st = PackState::new();
    let data = stereo_mute_frame();
    let fidx = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    {
        let fr = &mut st.frames[fidx];
        fr.privbit = 1;
        fr.original = 0;
        fr.copyright = 1;
        fr.bit_reservoir = 300;
    }
    mp3_mute_frame(&mut st, fidx);
    st.g_nchannels = 2;
    st.n_bad_first = 0;
    assert!(mp3_unmute_frame(&mut st, fidx));
    // stored byte0 = 0x80|0x00|0x20|(300>>8)&1 = 0xA1, byte1 = 0x2C
    assert_eq!(st.frames[fidx].bit_reservoir, ((0xA1_i32) << 8 | 0x2C) as i16);
}

#[test]
fn decode_frame_decodes_simple_long_block() {
    // Build a frame whose side info says granule(0,0) has 1 huffman-coded
    // big-value pair (2 coefficients) using table 1 (linbits 0), and the rest
    // is silent. HTAB1 code for (1,0) is `01` (2 bits), then a sign bit.
    // main data bits for granule(0,0): "01" + sign(1) = "011" -> coef[0] = -1.
    let mut side = vec![0u8; 32];
    // helper bit writer
    fn put(side: &mut [u8], bitpos: usize, bits: u32, n: usize) -> usize {
        let mut p = bitpos;
        for i in (0..n).rev() {
            let b = ((bits >> i) & 1) as u8;
            side[p / 8] |= b << (7 - (p % 8));
            p += 1;
        }
        p
    }
    let mut bp = 0;
    bp = put(&mut side, bp, 0, 9);
    bp = put(&mut side, bp, 0, 3);
    bp = put(&mut side, bp, 0, 4);
    bp = put(&mut side, bp, 0, 4);
    for gr in 0..2 {
        for ch in 0..2 {
            let mdb = if ch == 0 && gr == 0 { 3 } else { 0 };
            let bvp = if ch == 0 && gr == 0 { 1 } else { 0 };
            bp = put(&mut side, bp, mdb, 12);
            bp = put(&mut side, bp, bvp, 9);
            bp = put(&mut side, bp, 0, 8); // global_gain
            bp = put(&mut side, bp, 0, 4); // slength
            bp = put(&mut side, bp, 0, 1); // window_switching
            bp = put(&mut side, bp, 1, 5); // region_table[0] = 1 (HTAB1)
            bp = put(&mut side, bp, 0, 5); // region_table[1]
            bp = put(&mut side, bp, 0, 5); // region_table[2]
            bp = put(&mut side, bp, 0, 4); // region0_size
            bp = put(&mut side, bp, 0, 3); // region1_size
            bp = put(&mut side, bp, 0, 1); // preemphasis
            bp = put(&mut side, bp, 0, 1); // coarse
            bp = put(&mut side, bp, 0, 1); // select_htabB
        }
    }
    assert_eq!(bp, 256);

    let mut data = vec![0u8; 417];
    data[0..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x40]);
    data[4..4 + 32].copy_from_slice(&side);
    // granule(0,0) main data starts at fixed_size=36: "011" = 0b011
    data[36] = 0b0110_0000;

    let mut st = PackState::new();
    let fidx = mp3_read_frame(&mut st, &data, data.len() as i32).unwrap();
    st.frames[fidx].main_index = 36; // where main data begins (set by caller in C++)

    // decode the frame; the reader is over the whole buffer so that the
    // main_index-based setpos() lands on the right byte
    let mut h = HuffmanReader::new(&data);
    let fd = mp3_decode_frame(&mut st, &mut h, fidx);

    // granule(0,0): coef[0] = -1 (HTAB1 (1,0) with sign 1), rest 0
    assert_eq!(fd[0][0].coefficients[0], -1);
    assert_eq!(fd[0][0].coefficients[1], 0);
    assert_eq!(fd[0][0].coefficients[2], 0);
    // sv_bound counts decoded coefficients (1 pair = 2)
    assert_eq!(st.frames[fidx].granule(&st, 0, 0).sv_bound, 2);
    // all other granules silent
    for (ch, row) in fd.iter().enumerate() {
        for (gr, gd) in row.iter().enumerate() {
            if ch == 0 && gr == 0 {
                continue;
            }
            assert_eq!(gd.coefficients[0], 0);
            assert_eq!(st.frames[fidx].granule(&st, ch, gr).sv_bound, 0);
        }
    }
}
