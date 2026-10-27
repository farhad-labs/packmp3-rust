// testsupport.rs - helpers to synthesize MP3 test files.
//
// Used by the integration tests and the `genmp3` example. Kept in the library
// so it is available to examples without `#[cfg(test)]` gating.

use crate::huffmp3::HuffmanWriter;
use crate::hufftables::{BV_ENC_TABLE, HTABA_ENC, HTABB_ENC};
use crate::mp3;
use crate::state::PackState;
use crate::tables::{
    LAYER_III, MP3_MONO, MP3_STEREO, MP3_V1_0, SCF_WIDTH, SLENGTH_TABLE, mp3_bandwidth_bounds,
    mp3_frame_size_table,
};

/// deterministic xorshift32 PRNG for filling synthetic spectra
pub struct TestRng(u32);

impl TestRng {
    pub fn new(seed: u32) -> Self {
        TestRng(if seed == 0 { 0x9E3779B9 } else { seed })
    }
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
}

/// Build a silent MPEG-1 Layer III file with `nframes` identical frames.
/// `mono` selects mono (1 channel) vs stereo (2 channels). Frames are CBR
/// 128 kbps @ 44100 Hz with no CRC and a constant zero bit reservoir.
pub fn build_silent_mp3(nframes: i32, mono: bool) -> Vec<u8> {
    let mut st = PackState::new();

    // frame header parameters (shared by all frames)
    st.ai.i_mpeg = MP3_V1_0 as i8;
    st.ai.i_layer = LAYER_III as i8;
    st.ai.i_protection = 0; // no crc
    st.ai.i_bitrate = 9; // 128 kbps
    st.ai.i_samplerate = 0; // 44100 Hz
    st.ai.i_padding = 0;
    st.ai.i_privbit = 0;
    st.ai.i_channels = if mono { MP3_MONO as i8 } else { MP3_STEREO as i8 };
    st.ai.i_stereo_ms = 0;
    st.ai.i_stereo_int = 0;
    st.ai.i_copyright = 0;
    st.ai.i_original = 1;
    st.ai.i_emphasis = 0;
    st.ai.i_padbits = 0;
    st.ai.i_bit_res = 0;
    st.ai.i_share = 0;
    st.ai.i_sblocks = 0;
    st.ai.i_mixed = 0;
    st.ai.i_preemphasis = 0;
    st.ai.i_coarse = 0;
    st.ai.i_sbgain = 0;
    st.ai.i_aux_h = 0;
    st.ai.i_sb_diff = 0;

    let frame_size = mp3_frame_size_table(0)[9];

    for _ in 0..nframes {
        let fi = mp3::mp3_build_frame(&mut st).unwrap();
        st.frames[fi].frame_size = frame_size;
        // silent granules: no main data, no coefficients, no scalefactors
        let nch = st.frames[fi].nchannels as usize;
        for ch in 0..nch {
            for gr in 0..2 {
                let g = &mut st.granules[st.frames[fi].granules[ch][gr]];
                g.main_data_bit = 0;
                g.big_val_pairs = 0;
                g.global_gain = 0;
                g.slength = 0;
                g.select_htabb = 0;
            }
        }
        st.frames[fi].main_bits = 0;
        st.frames[fi].main_size = 0;
        st.frames[fi].aux_size = st.frames[fi].frame_size - st.frames[fi].fixed_size;
        mp3::mp3_append_frame(&mut st, fi);
    }

    // serialize fixed part + zero aux data
    let mut out = Vec::new();
    let mut fi = st.firstframe;
    while let Some(f) = fi {
        let fixed = mp3::mp3_build_fixed(&mut st, f);
        out.extend_from_slice(&fixed);
        out.extend(vec![0u8; st.frames[f].aux_size as usize]);
        fi = st.frames[f].next;
    }
    out
}

/// huffman tables used for each of the three spectral regions
const REGION0_TABLES: [i32; 4] = [1, 2, 3, 5];
// table 16 (linbits=1) exercises the escape path; 13/15 are full-range tables
const REGION1_TABLES: [i32; 6] = [5, 7, 10, 13, 16, 3];
// tables with linbits dominate the top region so escape values occur
const REGION2_TABLES: [i32; 6] = [16, 17, 24, 25, 15, 1];

/// Build an MPEG-1 Layer III file whose main data carries real huffman-coded
/// spectral data: varied scalefactors, big values across all three regions
/// (including linbits escape pairs), and count1 quadruples, plus sign bits.
///
/// `bitrate_idx` is the MPEG-1 Layer III bitrate table index (1..14, e.g. 9 =
/// 128 kbps); frames use a fixed CBR size for that index. Everything is
/// deterministic for a given `seed`. `mono` selects the 1-channel (17-byte
/// side info, j_coding off) layout. This is the "hard" counterpart to
/// `build_silent_mp3`: the coefficient stream really is huffman coded, so the
/// whole read->compress->decompress->write pipeline must reproduce the exact
/// bits of the original file.
pub fn build_huffman_mp3(seed: u32, nframes: i32, mono: bool, bitrate_idx: i32) -> Vec<u8> {
    let mut st = PackState::new();
    let mut rng = TestRng::new(seed);

    // frame header parameters (shared by all frames)
    st.ai.i_mpeg = MP3_V1_0 as i8;
    st.ai.i_layer = LAYER_III as i8;
    st.ai.i_protection = 0; // no crc
    st.ai.i_bitrate = bitrate_idx as i8;
    st.ai.i_samplerate = 0; // 44100 Hz
    st.ai.i_padding = 0;
    st.ai.i_privbit = 0;
    st.ai.i_channels = if mono { MP3_MONO as i8 } else { MP3_STEREO as i8 };
    st.ai.i_stereo_ms = 0;
    st.ai.i_stereo_int = 0;
    st.ai.i_copyright = 0;
    st.ai.i_original = 1;
    st.ai.i_emphasis = 0;
    st.ai.i_padbits = 0;
    st.ai.i_bit_res = 0;
    st.ai.i_share = 0;
    st.ai.i_sblocks = 0;
    st.ai.i_mixed = 0;
    st.ai.i_preemphasis = 0;
    st.ai.i_coarse = 0;
    st.ai.i_sbgain = 0;
    st.ai.i_aux_h = 0;
    st.ai.i_sb_diff = 0;

    let frame_size = mp3_frame_size_table(0)[bitrate_idx as usize];
    // bandwidth bounds for MPEG-1, 44.1 kHz (== st.ai.i_samplerate = 0)
    let bounds = mp3_bandwidth_bounds(0);

    let mut out = Vec::new();
    for _ in 0..nframes {
        let fi = mp3::mp3_build_frame(&mut st).unwrap();
        st.frames[fi].frame_size = frame_size;

        // the whole frame's main data goes into one bit stream (granule
        // boundaries are not byte aligned, exactly like a real MP3 file)
        let mut hw = HuffmanWriter::new(0);
        let nch = st.frames[fi].nchannels as usize;
        // whole-frame main-data budget in bits; each of the nch*2 granules
        // shares it
        let aux_cap_bits = (frame_size - st.frames[fi].fixed_size) * 8;
        for ch in 0..nch {
            for gr in 0..2 {
                // ---- pick side-info parameters ----
                let global_gain = (rng.next_u32() & 0xFF) as i16;
                let slength = [1i8, 5, 9, 14][(rng.next_u32() % 4) as usize];
                let r0s = 1 + (rng.next_u32() % 2); // 1..=2
                let r1s = rng.next_u32() % 2; // 0..=1

                // scale the amount of coded data to the frame budget so the
                // synthetic file stays valid at low bitrates as well
                let per_granule = aux_cap_bits / (nch as i32) / 2;
                let slen = SLENGTH_TABLE[slength as usize];
                let scf_bits: i32 = (0..4)
                    .map(|g| SCF_WIDTH[g as usize] * slen[if g < 2 { 0 } else { 1 }])
                    .sum();
                let mut quad_count = (1 + (rng.next_u32() % 6)) as i32;
                // conservative per-pair/per-quad bit costs incl. forced escape
                let mut bvp = (8 + (rng.next_u32() % 9)) as i32; // 8..=16 pairs
                let max_pairs = ((per_granule - scf_bits - quad_count * 12) / 20).clamp(1, 20);
                if bvp > max_pairs {
                    bvp = max_pairs;
                }
                while quad_count > 1 && scf_bits + bvp * 20 + quad_count * 12 > per_granule {
                    quad_count -= 1;
                }

                let select_htabb = (rng.next_u32() & 1) as i8;

                let rt0 = REGION0_TABLES[(rng.next_u32() % REGION0_TABLES.len() as u32) as usize];
                let rt1 = REGION1_TABLES[(rng.next_u32() % REGION1_TABLES.len() as u32) as usize];
                let rt2 = REGION2_TABLES[(rng.next_u32() % REGION2_TABLES.len() as u32) as usize];

                // region bounds exactly as mp3_read_frame() derives them
                let rb2 = bvp << 1;
                let mut rb0 = bounds[(r0s as usize) + 1];
                let mut rb1 = bounds[(r0s as usize) + (r1s as usize) + 2];
                if rb0 > rb2 {
                    rb0 = rb2;
                    rb1 = rb2;
                } else if rb1 > rb2 {
                    rb1 = rb2;
                }

                hw.reset_counter();

                // ---- scalefactors (long blocks) ----
                let slen = SLENGTH_TABLE[slength as usize];
                for g in 0..4 {
                    let sl = slen[if g < 2 { 0 } else { 1 }];
                    if sl > 0 {
                        for _ in 0..SCF_WIDTH[g as usize] {
                            hw.write_bits(rng.next_u32() % (1u32 << sl), sl);
                        }
                    }
                }

                // ---- big values, region by region ----
                let rt = [rt0, rt1, rt2];
                let rup = [rb0, rb1, rb2];
                let rlb = [0, rb0, rb1];
                let mut k = 0; // pair index (coefficient positions 2k, 2k+1)
                for r in 0..3 {
                    let table = BV_ENC_TABLE[rt[r] as usize];
                    let hcodes = table.h.unwrap();
                    let linbits = table.linbits as i32;
                    // huffman symbols are 0..15 regardless of linbits
                    let maxv = table.max.min(15);
                    while 2 * k < rup[r] {
                        // first pair of the top region gets an escape symbol so
                        // the linbits path is always exercised when linbits > 0
                        let a: u8 = if r == 2 && k * 2 == rlb[2] && linbits > 0 {
                            15
                        } else {
                            (rng.next_u32() % (maxv + 1)) as u8
                        };
                        let b: u8 = (rng.next_u32() % (maxv + 1)) as u8;
                        hw.encode_pair(hcodes, &[a, b]);
                        for v in [a, b] {
                            if v > 0 {
                                if linbits > 0 && v == 15 {
                                    // non-zero lbt exercises the len model
                                    let lbt = 1 + (rng.next_u32() % ((1u32 << linbits) - 1));
                                    hw.write_bits(lbt, linbits);
                                }
                                hw.write_bit((rng.next_u32() & 1) as u8);
                            }
                        }
                        k += 1;
                    }
                }

                // ---- small values (count1 quadruples) ----
                let quad_end = rb2 + 4 * quad_count;
                let hcode = if select_htabb != 0 { HTABB_ENC } else { HTABA_ENC };
                let mut qp = rb2;
                while qp < quad_end {
                    let quad = [
                        (rng.next_u32() & 1) as u8,
                        (rng.next_u32() & 1) as u8,
                        (rng.next_u32() & 1) as u8,
                        (rng.next_u32() & 1) as u8,
                    ];
                    hw.encode_quadruple(hcode, &quad);
                    for v in quad {
                        if v != 0 {
                            hw.write_bit((rng.next_u32() & 1) as u8);
                        }
                    }
                    qp += 4;
                }

                let main_data_bit = hw.get_count() as i16;

                // ---- record granule state (used by mp3_build_fixed) ----
                let gi = st.frames[fi].granules[ch][gr];
                let g = &mut st.granules[gi];
                g.global_gain = global_gain;
                g.slength = slength;
                g.big_val_pairs = bvp as i16;
                g.region0_size = r0s as i8;
                g.region1_size = r1s as i8;
                g.region_table = [rt[0] as i8, rt[1] as i8, rt[2] as i8];
                g.select_htabb = select_htabb;
                g.main_data_bit = main_data_bit;
            }
        }
        // guard: the generated main data must fit inside the frame's aux bytes
        assert!(
            hw.get_count() <= aux_cap_bits,
            "synthetic huffman main data overflowed frame budget"
        );

        // assemble: fixed part + main data (padding bits beyond the granule
        // data fill the rest of the frame; `getptr()` pads the partial byte
        // with 1s, the remainder is zero)
        let fixed = mp3::mp3_build_fixed(&mut st, fi);
        let aux_size = frame_size - st.frames[fi].fixed_size;
        let mut aux = hw.getptr();
        aux.resize(aux_size as usize, 0);
        out.extend_from_slice(&fixed);
        out.extend_from_slice(&aux);
    }
    out
}
