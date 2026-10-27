// pmp.rs - PMP encode/decode streams.
// Faithful port of the PMP-specific functions in source/packmp3.cpp.
//
// The C++ code uses function-local `static` variables (pad_and_aux, LAME
// predictor) and pointer-linked frame/granule chains. We store the LAME
// predictor inside `PackState` (see state.rs) and iterate the arena chains via
// index vectors built by frame_chain()/granule_chain().
//
// Several C++ model_s/model_b pointers alias each other (e.g. the
// `mod_abv[..][..][17..23]` entries point to the same model as [16]), which is
// required for byte-identical output. We model that with Rc<RefCell<..>>.
#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]

use std::cell::RefCell;
use std::rc::Rc;

use crate::aricoder::{Aricoder, ModelB, ModelS, decode_ari_b, decode_ari_s, encode_ari_b, encode_ari_s};
use crate::bitops::{AbyteWriter, IoStream, MODE_READ, MODE_WRITE};
use crate::huffmp3::{HuffmanReader, HuffmanWriter};
use crate::hufftables::{
    BV_DEC_TABLE, BV_ENC_TABLE, HTABA_DEC, HTABB_DEC, HTABA_ENC, HTABB_ENC,
};
use crate::state::{LameAncPredictor, PackState, EL_FATAL, EL_WARNING};
use crate::tables::{
    AUX_DATA_STEP, GG_CONTEXT_SIZE, LAYER_III, LONG_BLOCK, MP3_JOINT_STEREO, MP3_MONO, MP3_STEREO,
    MP3_V1_0, PADDING_MAX_RUN, SCF_BOUNDS, SCF_LH_BOUNDS_SHORT, SCF_LH_WIDTH_SHORT, SCF_WIDTH,
    SHORT_BLOCK, SLENGTH_TABLE, START_BLOCK, STOP_BLOCK, STUFFING_STEP, SWITCHING_MAX_RUN,
    bitlen8192p, mp3_bitrate_table, mp3_frame_size_table, mp3_samplerate_table,
    mp3_bandwidth_bounds, mp3_bandwidth_bounds_short, MP3_BITRATE_PRED,
};

pub const APP_VERSION: u8 = 10;
pub const PMP_MAGIC: [u8; 2] = *b"MS";
pub const APP_NAME: &str = "packMP3";

const MEM_ERRMSG: &str = "out of memory error";

// ---------------------------------------------------------------------------
// model helpers (mirror INIT_MODEL_S / INIT_MODEL_B macros with c_lim = 511)
// ---------------------------------------------------------------------------

#[inline]
fn mod_s(max_s: i32, max_c: i32, max_o: i32) -> ModelS {
    ModelS::new(max_s, max_c, max_o, 511)
}

#[inline]
fn mod_b(max_c: i32, max_o: i32) -> ModelB {
    ModelB::new(max_c, max_o, 511)
}

#[inline]
fn wrap_s(m: ModelS) -> Rc<RefCell<ModelS>> {
    Rc::new(RefCell::new(m))
}

#[inline]
fn wrap_b(m: ModelB) -> Rc<RefCell<ModelB>> {
    Rc::new(RefCell::new(m))
}

#[inline]
fn enc_s(enc: &mut Aricoder, m: &Rc<RefCell<ModelS>>, c: i32) {
    encode_ari_s(enc, &mut m.borrow_mut(), c);
}

#[inline]
fn dec_s(dec: &mut Aricoder, m: &Rc<RefCell<ModelS>>) -> i32 {
    decode_ari_s(dec, &mut m.borrow_mut())
}

#[inline]
fn shift_s(m: &Rc<RefCell<ModelS>>, c: i32) {
    m.borrow_mut().shift_context(c);
}

#[inline]
fn shift_s2(m: &Rc<RefCell<ModelS>>, c1: i32, c2: i32) {
    let mut b = m.borrow_mut();
    b.shift_context(c1);
    b.shift_context(c2);
}

#[inline]
fn enc_b(enc: &mut Aricoder, m: &Rc<RefCell<ModelB>>, c: i32) {
    encode_ari_b(enc, &mut m.borrow_mut(), c);
}

#[inline]
fn dec_b(dec: &mut Aricoder, m: &Rc<RefCell<ModelB>>) -> i32 {
    decode_ari_b(dec, &mut m.borrow_mut())
}

#[inline]
fn shift_b(m: &Rc<RefCell<ModelB>>, c: i32) {
    m.borrow_mut().shift_context(c);
}

#[inline]
fn shift_b2(m: &Rc<RefCell<ModelB>>, c1: i32, c2: i32) {
    let mut b = m.borrow_mut();
    b.shift_context(c1);
    b.shift_context(c2);
}

// ---------------------------------------------------------------------------
// chain helpers (list arena indices in linked-list order)
// ---------------------------------------------------------------------------

pub(crate) fn frame_chain(st: &PackState) -> Vec<usize> {
    let mut v = Vec::new();
    let mut fi = st.firstframe;
    while let Some(f) = fi {
        v.push(f);
        fi = st.frames[f].next;
    }
    v
}

pub(crate) fn granule_chain(st: &PackState, ch: usize) -> Vec<usize> {
    let mut v = Vec::new();
    let first = st.firstframe.unwrap();
    let mut gi = st.frames[first].granules[ch][0];
    loop {
        v.push(gi);
        match st.granules[gi].next {
            Some(n) => gi = n,
            None => break,
        }
    }
    v
}

// ---------------------------------------------------------------------------
// PMP header
// ---------------------------------------------------------------------------

/// writes the PMP file header
pub fn pmp_write_header(st: &PackState, str_out: &mut IoStream) -> bool {
    let mut header = [0u8; 4];
    let mut nframes = [0u8; 4];

    // 1st byte: samplerate, channel mode, bitrate (zero if vbr/not global)
    header[0] |= ((st.ai.i_samplerate as i32) << 6) as u8; // sample rate
    header[0] |= ((st.ai.i_channels as i32) << 4) as u8; // channel mode
    header[0] |= (if st.ai.i_bitrate != -1 { st.ai.i_bitrate as i32 } else { 0 }) as u8; // bitrate
    // 2nd byte: padding, ms/int stereo, special blocks, subblock gain, sharing,
    // preemphasis and coarse scalefactors
    header[1] |= ((if st.ai.i_padding == 0 { 0 } else { 1 }) << 7) as u8;
    header[1] |= ((if st.ai.i_stereo_ms == 0 { 0 } else { 1 }) << 6) as u8;
    header[1] |= ((if st.ai.i_stereo_int == 0 { 0 } else { 1 }) << 5) as u8;
    header[1] |= ((if st.ai.i_sblocks == 0 { 0 } else { 1 }) << 4) as u8;
    header[1] |= ((if st.ai.i_sbgain == 0 { 0 } else { 1 }) << 3) as u8;
    header[1] |= ((if st.ai.i_share == 0 { 0 } else { 1 }) << 2) as u8;
    header[1] |= ((if st.ai.i_preemphasis == 0 { 0 } else { 1 }) << 1) as u8;
    header[1] |= (if st.ai.i_coarse == 0 { 0 } else { 1 }) as u8;
    // 3rd byte: protection, original, copyright, private bit, emphasis and
    // indicators for data before/after, special block diffs, bad first frames
    header[2] |= ((st.ai.i_protection as i32) << 7) as u8;
    header[2] |= ((st.ai.i_original as i32) << 6) as u8;
    header[2] |= ((st.ai.i_copyright as i32) << 5) as u8;
    header[2] |= ((st.ai.i_privbit as i32) << 4) as u8;
    header[2] |= ((st.ai.i_emphasis as i32) << 2) as u8;
    header[2] |= ((if st.data_before_size > 0 { 1 } else { 0 }) << 1) as u8;
    header[2] |= (if st.data_after_size > 0 { 1 } else { 0 }) as u8;
    // 4th byte: bit reservoir, special block diffs, bad first frames
    header[3] |= ((if st.ai.i_bit_res == 0 { 0 } else { 1 }) << 7) as u8;
    header[3] |= ((if st.ai.i_sb_diff == 0 { 0 } else { 1 }) << 6) as u8;
    header[3] |= ((if st.n_bad_first > 0 { 1 } else { 0 }) << 5) as u8;

    // store # of frames in nframes[] in little endian
    nframes[0] = ((st.g_nframes >> 24) & 0xFF) as u8;
    nframes[1] = ((st.g_nframes >> 16) & 0xFF) as u8;
    nframes[2] = ((st.g_nframes >> 8) & 0xFF) as u8;
    nframes[3] = (st.g_nframes & 0xFF) as u8;

    str_out.write(&header, 1, 4);
    str_out.write(&nframes, 1, 4);

    true
}

/// reads the PMP file header
pub fn pmp_read_header(st: &mut PackState, str_in: &mut IoStream) -> bool {
    let mut header = [0u8; 4];
    let mut nframes = [0u8; 4];

    // read header and size from file
    str_in.read(&mut header, 1, 4);
    str_in.read(&mut nframes, 1, 4);
    if str_in.chkeof() {
        st.errormessage = "unexpected end of data".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }

    // 1st byte
    st.ai.i_samplerate = ((header[0] >> 6) & 0x3) as i8;
    st.ai.i_channels = ((header[0] >> 4) & 0x3) as i8;
    st.ai.i_bitrate = (header[0] & 0xF) as i8;
    if st.ai.i_bitrate == 0 {
        st.ai.i_bitrate = -1;
    }
    // 2nd byte
    st.ai.i_padding = -(((header[1] >> 7) & 0x1) as i8);
    st.ai.i_stereo_ms = -(((header[1] >> 6) & 0x1) as i8);
    st.ai.i_stereo_int = ((header[1] >> 5) & 0x1) as i8;
    st.ai.i_sblocks = ((header[1] >> 4) & 0x1) as i8;
    st.ai.i_sbgain = -(((header[1] >> 3) & 0x1) as i8);
    st.ai.i_share = -(((header[1] >> 2) & 0x1) as i8);
    st.ai.i_preemphasis = -(((header[1] >> 1) & 0x1) as i8);
    st.ai.i_coarse = -((header[1] & 0x1) as i8);
    // 3rd byte
    st.ai.i_protection = ((header[2] >> 7) & 0x1) as i8;
    st.ai.i_original = ((header[2] >> 6) & 0x1) as i8;
    st.ai.i_copyright = ((header[2] >> 5) & 0x1) as i8;
    st.ai.i_privbit = ((header[2] >> 4) & 0x1) as i8;
    st.ai.i_emphasis = ((header[2] >> 2) & 0x3) as i8;
    st.data_before_size = ((header[2] >> 1) & 0x1) as i32;
    st.data_after_size = (header[2] & 0x1) as i32;
    // 4th byte
    st.ai.i_bit_res = -(((header[3] >> 7) & 0x1) as i8);
    st.ai.i_sb_diff = ((header[3] >> 6) & 0x1) as i8;
    st.n_bad_first = ((header[3] >> 5) & 0x1) as i32;

    // set nframes known i_variables
    st.ai.i_mpeg = MP3_V1_0 as i8;
    st.ai.i_layer = LAYER_III as i8;
    st.ai.i_padbits = 0;
    st.ai.i_mixed = 0;
    st.ai.i_aux_h = -2;

    // check for possible mistakes made by myself
    if st.ai.i_bitrate == 0xF || st.ai.i_samplerate == 0x3 {
        st.errormessage = "not a proper PMP file".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }

    // extract # of frames from nframes[] in little endian
    st.g_nframes = 0;
    st.g_nframes |= (nframes[0] as i32) << 24;
    st.g_nframes |= (nframes[1] as i32) << 16;
    st.g_nframes |= (nframes[2] as i32) << 8;
    st.g_nframes |= nframes[3] as i32;

    // build frames structure
    for _ in 0..st.g_nframes {
        // read frame, check result
        let frame = match crate::mp3::mp3_build_frame(st) {
            Some(f) => f,
            None => return false, // bad result -> use existing error message
        };
        crate::mp3::mp3_append_frame(st, frame);
    }

    // make some safe assumptions
    st.g_nchannels = if st.ai.i_channels == MP3_MONO as i8 { 1 } else { 2 };
    st.g_samplerate = mp3_samplerate_table(st.ai.i_samplerate as i32);
    st.g_bitrate = if st.ai.i_bitrate == -1 {
        0
    } else {
        mp3_bitrate_table(st.ai.i_bitrate as i32)
    };

    true
}

// ---------------------------------------------------------------------------
// ID3 data (generic data stream, arithmetic coded)
// ---------------------------------------------------------------------------

/// encodes a stream of generic data (8bit)
pub fn pmp_encode_id3(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut model = mod_s(256 + 1, 256, 0);
    // data before main data
    if st.data_before_size > 0 {
        for i in 0..st.data_before_size as usize {
            encode_ari_s(enc, &mut model, st.data_before[i] as i32);
        }
        // encode end-of-data symbol (256)
        encode_ari_s(enc, &mut model, 256);
    }
    // data after main data
    if st.data_after_size > 0 {
        for i in 0..st.data_after_size as usize {
            encode_ari_s(enc, &mut model, st.data_after[i] as i32);
        }
        // encode end-of-data symbol (256)
        encode_ari_s(enc, &mut model, 256);
    }
    true
}

/// decodes a stream of generic data (8 bit)
pub fn pmp_decode_id3(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut model = mod_s(256 + 1, 256, 0);

    // data before main data
    if st.data_before_size > 0 {
        let mut bwrt = AbyteWriter::new(1024);
        loop {
            let c = decode_ari_s(dec, &mut model);
            if c == 256 {
                break;
            }
            bwrt.write(c as u8);
        }
        // check for out of memory
        if bwrt.error {
            st.errormessage = MEM_ERRMSG.to_string();
            st.errorlevel = EL_FATAL;
            return false;
        }
        st.data_before = bwrt.getptr();
        st.data_before_size = bwrt.getpos() as i32;
    }

    // data after main data
    if st.data_after_size > 0 {
        let mut bwrt = AbyteWriter::new(1024);
        loop {
            let c = decode_ari_s(dec, &mut model);
            if c == 256 {
                break;
            }
            bwrt.write(c as u8);
        }
        // check for out of memory
        if bwrt.error {
            st.errormessage = MEM_ERRMSG.to_string();
            st.errorlevel = EL_FATAL;
            return false;
        }
        st.data_after = bwrt.getptr();
        st.data_after_size = bwrt.getpos() as i32;
    }

    true
}

// ---------------------------------------------------------------------------
// padding bit (run-length + arithmetic coding)
// ---------------------------------------------------------------------------

/// encode the padding bit (1 bit)
pub fn pmp_encode_padding(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut model = mod_s(PADDING_MAX_RUN + 1, PADDING_MAX_RUN + 1, 1);
    let chainf = frame_chain(st);
    let mut run = 0i32;
    let mut bit: i8 = 0;

    // encoding start
    for f in &chainf {
        if st.frames[*f].padding != bit {
            bit ^= 0x1;
            while run > PADDING_MAX_RUN {
                encode_ari_s(enc, &mut model, PADDING_MAX_RUN);
                model.shift_context(PADDING_MAX_RUN);
                encode_ari_s(enc, &mut model, 0);
                model.shift_context(0);
                run -= PADDING_MAX_RUN;
            }
            encode_ari_s(enc, &mut model, run);
            model.shift_context(run);
            run = 0;
        }
        run += 1;
    }
    // encode last run
    if run > 0 {
        while run > PADDING_MAX_RUN {
            encode_ari_s(enc, &mut model, PADDING_MAX_RUN);
            model.shift_context(PADDING_MAX_RUN);
            encode_ari_s(enc, &mut model, 0);
            model.shift_context(0);
            run -= PADDING_MAX_RUN;
        }
        encode_ari_s(enc, &mut model, run);
    }

    true
}

/// decode the padding bit (1 bit)
pub fn pmp_decode_padding(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut model = mod_s(PADDING_MAX_RUN + 1, PADDING_MAX_RUN + 1, 1);
    let chainf = frame_chain(st);

    // decode the first run
    let mut run = decode_ari_s(dec, &mut model);
    model.shift_context(run);
    let mut bit: i8 = 0;
    // ... and all others
    for f in &chainf {
        while run == 0 {
            bit ^= 0x1;
            run = decode_ari_s(dec, &mut model);
            model.shift_context(run);
        }
        run -= 1;
        st.frames[*f].padding = bit;
    }

    true
}

// ---------------------------------------------------------------------------
// block types (1-3 bit)
// ---------------------------------------------------------------------------

/// encode block types (1-3 bit)
pub fn pmp_encode_block_types(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut mod_sw = mod_s(SWITCHING_MAX_RUN + 1, SWITCHING_MAX_RUN + 1, 1);
    let mut mod_bt = mod_s(4, 4, 1);
    let mut ctx: i32;

    // encoding start
    for ch in 0..st.g_nchannels as usize {
        if st.ai.i_sb_diff == 0 && ch == 1 {
            break;
        }
        mod_sw.shift_context(0);
        let mut run = 0i32;
        let mut bit: i8 = 0;
        // first step - encode the switching runs
        let chain = granule_chain(st, ch);
        for gi in &chain {
            if st.granules[*gi].window_switching != bit {
                bit ^= 0x1;
                while run > SWITCHING_MAX_RUN {
                    encode_ari_s(enc, &mut mod_sw, SWITCHING_MAX_RUN);
                    mod_sw.shift_context(SWITCHING_MAX_RUN);
                    encode_ari_s(enc, &mut mod_sw, 0);
                    mod_sw.shift_context(0);
                    run -= SWITCHING_MAX_RUN;
                }
                encode_ari_s(enc, &mut mod_sw, run);
                mod_sw.shift_context(run);
                run = 0;
            }
            run += 1;
        }
        if run > 0 {
            // encode last run
            while run > SWITCHING_MAX_RUN {
                encode_ari_s(enc, &mut mod_sw, SWITCHING_MAX_RUN);
                mod_sw.shift_context(SWITCHING_MAX_RUN);
                encode_ari_s(enc, &mut mod_sw, 0);
                mod_sw.shift_context(0);
                run -= SWITCHING_MAX_RUN;
            }
            encode_ari_s(enc, &mut mod_sw, run);
            mod_sw.shift_context(run);
        }
        // second step - encode the block types
        let mut c = STOP_BLOCK;
        for gi in &chain {
            if st.granules[*gi].window_switching != 0 {
                if c == STOP_BLOCK {
                    ctx = START_BLOCK;
                } else if st.granules[*gi].next.is_some() {
                    let next = st.granules[*gi].next.unwrap();
                    ctx = if st.granules[next].window_switching != 0 {
                        SHORT_BLOCK
                    } else {
                        STOP_BLOCK
                    };
                } else {
                    ctx = STOP_BLOCK;
                }
                mod_bt.shift_context(ctx);
                c = st.granules[*gi].block_type as i32;
                encode_ari_s(enc, &mut mod_bt, c);
            }
        }
    }

    true
}

/// decode block types (1-3 bit)
pub fn pmp_decode_block_types(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut mod_sw = mod_s(SWITCHING_MAX_RUN + 1, SWITCHING_MAX_RUN + 1, 1);
    let mut mod_bt = mod_s(4, 4, 1);

    // decoding start
    for ch in 0..st.g_nchannels as usize {
        if st.ai.i_sb_diff == 0 && ch == 1 {
            break;
        }
        mod_sw.shift_context(0);
        let chain: Vec<usize> = {
            let first = st.firstframe.unwrap();
            let mut v = Vec::new();
            let mut gi = st.frames[first].granules[ch][0];
            loop {
                v.push(gi);
                match st.granules[gi].next {
                    Some(n) => gi = n,
                    None => break,
                }
            }
            v
        };
        // first step - decode the switching runs
        let mut run = decode_ari_s(dec, &mut mod_sw);
        mod_sw.shift_context(run);
        let mut bit: i8 = 0;
        for gi in &chain {
            while run == 0 {
                bit ^= 0x1;
                run = decode_ari_s(dec, &mut mod_sw);
                mod_sw.shift_context(run);
            }
            run -= 1;
            st.granules[*gi].window_switching = bit;
        }
        // second step - decode the block types
        let mut c = STOP_BLOCK;
        for gi in &chain {
            if st.granules[*gi].window_switching != 0 {
                let ctx = if c == STOP_BLOCK {
                    START_BLOCK
                } else if st.granules[*gi].next.is_some() {
                    let next = st.granules[*gi].next.unwrap();
                    if st.granules[next].window_switching != 0 {
                        SHORT_BLOCK
                    } else {
                        STOP_BLOCK
                    }
                } else {
                    STOP_BLOCK
                };
                mod_bt.shift_context(ctx);
                c = decode_ari_s(dec, &mut mod_bt);
                st.granules[*gi].block_type = c as i8;
            } else {
                st.granules[*gi].block_type = LONG_BLOCK as i8;
            }
        }
    }

    // copy ch1 block types to ch0 if identical
    if st.ai.i_sb_diff == 0 && st.g_nchannels == 2 {
        let chain0 = granule_chain(st, 0);
        let chain1 = granule_chain(st, 1);
        for (g0, g1) in chain0.iter().zip(chain1.iter()) {
            st.granules[*g1].window_switching = st.granules[*g0].window_switching;
            st.granules[*g1].block_type = st.granules[*g0].block_type;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// global gain (8 bit, differential)
// ---------------------------------------------------------------------------

/// encode the global gain (8 bit)
pub fn pmp_encode_global_gain(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut model = mod_s(256, 0, 0);
    let chain0 = granule_chain(st, 0);

    // --- first channel ---
    let mut last = 0i32;
    for gi in &chain0 {
        let gg = st.granules[*gi].global_gain as i32;
        let c = (gg - last) & 0xFF;
        last = gg;
        encode_ari_s(enc, &mut model, c);
    }

    // --- second channel ---
    if st.g_nchannels == 2 {
        let chain1 = granule_chain(st, 1);
        for (g0, g1) in chain0.iter().zip(chain1.iter()) {
            let gg1 = st.granules[*g1].global_gain as i32;
            let gg0 = st.granules[*g0].global_gain as i32;
            let c = (gg1 - gg0) & 0xFF;
            encode_ari_s(enc, &mut model, c);
        }
    }

    true
}

/// decode the global gain (8 bit)
pub fn pmp_decode_global_gain(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut model = mod_s(256, 0, 0);
    let chain0 = granule_chain(st, 0);

    // --- first channel ---
    let mut last = 0i32;
    for gi in &chain0 {
        let c = decode_ari_s(dec, &mut model);
        last = (c + last) & 0xFF;
        st.granules[*gi].global_gain = last as i16;
    }

    // --- second channel ---
    if st.g_nchannels == 2 {
        let chain1 = granule_chain(st, 1);
        for (g0, g1) in chain0.iter().zip(chain1.iter()) {
            let c = decode_ari_s(dec, &mut model);
            let gg0 = st.granules[*g0].global_gain as i32;
            st.granules[*g1].global_gain = ((c + gg0) & 0xFF) as i16;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// slength (4 bit)
// ---------------------------------------------------------------------------

/// encode the slength (4 bit)
pub fn pmp_encode_slength(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut model = mod_s(16, 16, 2);

    // encoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush model
        model.flush_model(1);
        let gg_ctx = st.gg_context[ch].as_ref().unwrap();
        let chain = granule_chain(st, ch);
        let mut c = 0i32;
        // encode one channel
        for (i, gi) in chain.iter().enumerate() {
            let gg = gg_ctx[i] as i32;
            model.shift_context(gg);
            model.shift_context(c);
            c = st.granules[*gi].slength as i32;
            encode_ari_s(enc, &mut model, c);
        }
    }

    true
}

/// decode the slength (4 bit)
pub fn pmp_decode_slength(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut model = mod_s(16, 16, 2);

    // decoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush model
        model.flush_model(1);
        let gg_ctx = st.gg_context[ch].as_ref().unwrap();
        let chain = granule_chain(st, ch);
        let mut c = 0i32;
        // decode one channel
        for (i, gi) in chain.iter().enumerate() {
            let gg = gg_ctx[i] as i32;
            model.shift_context(gg);
            model.shift_context(c);
            let v = decode_ari_s(dec, &mut model);
            st.granules[*gi].slength = v as i8;
            c = v;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// ms stereo setting (1 bit)
// ---------------------------------------------------------------------------

/// encode ms stereo setting (1 bit)
pub fn pmp_encode_stereo_ms(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut model = mod_b(16, 1);
    let chainf = frame_chain(st);
    let mut ctx: i32 = 0;

    // encoding start
    for f in &chainf {
        model.shift_context(ctx);
        encode_ari_b(enc, &mut model, st.frames[*f].stereo_ms as i32);
        ctx = ((ctx << 1) | st.frames[*f].stereo_ms as i32) & 0xF;
    }

    true
}

/// decode ms stereo setting (1 bit)
pub fn pmp_decode_stereo_ms(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut model = mod_b(16, 1);
    let chainf = frame_chain(st);
    let mut ctx: i32 = 0;

    // decoding start
    for f in &chainf {
        model.shift_context(ctx);
        let v = decode_ari_b(dec, &mut model);
        st.frames[*f].stereo_ms = v as i8;
        ctx = ((ctx << 1) | v) & 0xF;
    }

    true
}

// ---------------------------------------------------------------------------
// region bounds and tables (32 bit)
// ---------------------------------------------------------------------------

/// encode region bounds and tables (32 bit)
pub fn pmp_encode_region_data(st: &mut PackState, enc: &mut Aricoder) -> bool {
    let bw_conv: &[i32] = &crate::tables::MP3_BANDWIDTH_CONV[st.ai.i_samplerate as usize];

    let mod_t0 = wrap_s(mod_s(32, 32, 2));
    let mod_t1 = wrap_s(mod_s(32, 32, 2));
    let mod_t2 = wrap_s(mod_s(32, 32, 2));
    let mod_ts = wrap_b(mod_b(16, 1));
    let mod_s0 = wrap_s(mod_s(16, 22, 2));
    let mod_s1 = wrap_s(mod_s(8, 22, 2));
    let mod_bv = wrap_s(mod_s((576 / 2) + 1, 1 + 1, 1));

    // encoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush models
        mod_s0.borrow_mut().flush_model(1);
        mod_s1.borrow_mut().flush_model(1);
        mod_bv.borrow_mut().flush_model(1);
        // reset context
        let gg_ctx = st.gg_context[ch].as_ref().unwrap();
        let chain = granule_chain(st, ch);
        let mut ctx_sv: i32 = 0;
        let mut t_r0 = 0i32;
        let mut t_r1: i32;
        let mut t_r2: i32;
        let mut t_sv: i32;
        // s_r0 = 0; s_r1 = 0; s_r2 = 0;
        let mut s_r0 = 0i32;
        let mut s_r2: i32;

        for (i, gi) in chain.iter().enumerate() {
            {
                let big_val_pairs = st.granules[*gi].big_val_pairs as i32;
                let window_switching = st.granules[*gi].window_switching as i32;
                let block_type = st.granules[*gi].block_type as i32;
                let region0_size = st.granules[*gi].region0_size as i32;
                let region1_size = st.granules[*gi].region1_size as i32;
                let region_table = st.granules[*gi].region_table;

                // --- region bounds (big_val_pairs, region0_size, region2_size) ---
                if big_val_pairs > 576 / 2 {
                    // replicate C++ printf "(%c>%c)" with char of the values
                    let cg0 = (big_val_pairs & 0xFF) as u8 as char;
                    let cg1 = ((576 / 2) & 0xFF) as u8 as char;
                    st.errormessage = format!(
                        "big value pairs out of bounds ({}h>{})",
                        cg0, cg1
                    );
                    st.errorlevel = EL_FATAL;
                    return false;
                }
                if window_switching != 0 {
                    // region 2 size (# big value pairs)
                    shift_s(&mod_bv, if block_type == SHORT_BLOCK { 1 } else { 0 });
                    enc_s(enc, &mod_bv, big_val_pairs);
                    s_r0 = 0; // s_r1 = 0;
                } else {
                    s_r2 = bw_conv[(big_val_pairs << 1) as usize];
                    // region 2 size (# big value pairs)
                    shift_s(&mod_bv, 0);
                    enc_s(enc, &mod_bv, big_val_pairs);
                    // region 0 size
                    shift_s2(&mod_s0, s_r0, s_r2);
                    s_r0 = region0_size;
                    enc_s(enc, &mod_s0, s_r0);
                    // region 1 size
                    shift_s2(&mod_s1, s_r0, s_r2);
                    enc_s(enc, &mod_s1, region1_size);
                    // context customizations
                    s_r0 += 1; // s_r1 = s_r0 + granule->region1_size + 1;
                }
                // --- region tables (region0/1/2_table, select_htabB) ---
                // region 0 table
                shift_s2(&mod_t0, gg_ctx[i] as i32, t_r0);
                // shift_model( mod_t0, s_r0, t_r0 );
                t_r0 = region_table[0] as i32;
                enc_s(enc, &mod_t0, t_r0);
                // region 1 table
                shift_s2(&mod_t1, t_r0, s_r0);
                t_r1 = region_table[1] as i32;
                enc_s(enc, &mod_t1, t_r1);
                // region 2 table
                if window_switching == 0 {
                    shift_s2(&mod_t2, t_r0, t_r1);
                    t_r2 = region_table[2] as i32;
                    enc_s(enc, &mod_t2, t_r2);
                }
                // small values table
                shift_b(&mod_ts, ctx_sv);
                t_sv = st.granules[*gi].select_htabb as i32;
                enc_b(enc, &mod_ts, t_sv);
                ctx_sv = ((ctx_sv << 1) | t_sv) & 0xF;
                // advance context
            }
        }
    }

    true
}

/// decode region sizes and table selection (32 bit)
pub fn pmp_decode_region_data(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let bw_conv: &[i32] = &crate::tables::MP3_BANDWIDTH_CONV[st.ai.i_samplerate as usize];

    let mod_t0 = wrap_s(mod_s(32, 32, 2));
    let mod_t1 = wrap_s(mod_s(32, 32, 2));
    let mod_t2 = wrap_s(mod_s(32, 32, 2));
    let mod_ts = wrap_b(mod_b(16, 1));
    let mod_s0 = wrap_s(mod_s(16, 22, 2));
    let mod_s1 = wrap_s(mod_s(8, 22, 2));
    let mod_bv = wrap_s(mod_s((576 / 2) + 1, 1 + 1, 1));

    // decoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush model
        mod_s0.borrow_mut().flush_model(1);
        mod_s1.borrow_mut().flush_model(1);
        mod_bv.borrow_mut().flush_model(1);
        // reset context
        let gg_ctx = st.gg_context[ch].as_ref().unwrap();
        let chain = granule_chain(st, ch);
        let mut ctx_sv: i32 = 0;
        let mut t_r0 = 0i32;
        let mut t_r1: i32;
        let mut t_r2: i32;
        let mut t_sv: i32;
        // s_r0 = 0; s_r1 = 0; s_r2 = 0;
        let mut s_r0 = 0i32;
        let mut s_r2: i32;

        for (i, gi) in chain.iter().enumerate() {
            // --- region bounds (big_val_pairs, region0_size, region2_size) ---
            if st.granules[*gi].window_switching == 0 {
                // region 2
                shift_s(&mod_bv, 0);
                let bvp = dec_s(dec, &mod_bv);
                st.granules[*gi].big_val_pairs = bvp as i16;
                s_r2 = bw_conv[(bvp << 1) as usize];
                // region 0
                shift_s2(&mod_s0, s_r0, s_r2);
                s_r0 = dec_s(dec, &mod_s0);
                st.granules[*gi].region0_size = s_r0 as i8;
                // region 1
                shift_s2(&mod_s1, s_r0, s_r2);
                let r1 = dec_s(dec, &mod_s1);
                st.granules[*gi].region1_size = r1 as i8;
                // set region 1/2 bounds
                let bounds = mp3_bandwidth_bounds(st.ai.i_samplerate as i32);
                let b0 = bounds[(s_r0 + 1) as usize];
                let b1 = bounds[(s_r0 + r1 + 2) as usize];
                let b2 = bvp << 1;
                st.granules[*gi].region_bound[0] = b0 as i16;
                st.granules[*gi].region_bound[1] = b1 as i16;
                st.granules[*gi].region_bound[2] = b2 as i16;
                if st.granules[*gi].region_bound[0] > st.granules[*gi].region_bound[2] {
                    st.granules[*gi].region_bound[0] = st.granules[*gi].region_bound[2];
                    st.granules[*gi].region_bound[1] = st.granules[*gi].region_bound[2];
                } else if st.granules[*gi].region_bound[1] > st.granules[*gi].region_bound[2] {
                    st.granules[*gi].region_bound[1] = st.granules[*gi].region_bound[2];
                }
                // context customizations
                s_r0 += 1; // s_r1 = s_r0 + granule->region1_size + 1;
            } else if st.granules[*gi].block_type as i32 != SHORT_BLOCK {
                // region sizes for different block types
                st.granules[*gi].region0_size = 8;
                st.granules[*gi].region1_size = 0;
                // set region bound
                let bounds = mp3_bandwidth_bounds(st.ai.i_samplerate as i32);
                st.granules[*gi].region_bound[0] = bounds[8] as i16;
                // decode bv pairs, set bound and check r0 bound
                shift_s(&mod_bv, 0);
                let bvp = dec_s(dec, &mod_bv);
                st.granules[*gi].big_val_pairs = bvp as i16;
                st.granules[*gi].region_bound[1] = (bvp << 1) as i16;
                if st.granules[*gi].region_bound[0] > st.granules[*gi].region_bound[1] {
                    st.granules[*gi].region_bound[0] = st.granules[*gi].region_bound[1];
                }
                st.granules[*gi].region_bound[2] = st.granules[*gi].region_bound[1];
                // context setting
                s_r0 = 0; // s_r1 = 0;
            } else {
                // special treatment for mixed blocks needed (!)
                st.granules[*gi].region0_size = 9;
                st.granules[*gi].region1_size = 0;
                // set region bound
                let bounds = mp3_bandwidth_bounds_short(st.ai.i_samplerate as i32);
                st.granules[*gi].region_bound[0] = (bounds[9 / 3] * 3) as i16;
                // decode bv pairs, set bound and check r0 bound
                shift_s(&mod_bv, 1);
                let bvp = dec_s(dec, &mod_bv);
                st.granules[*gi].big_val_pairs = bvp as i16;
                st.granules[*gi].region_bound[1] = (bvp << 1) as i16;
                if st.granules[*gi].region_bound[0] > st.granules[*gi].region_bound[1] {
                    st.granules[*gi].region_bound[0] = st.granules[*gi].region_bound[1];
                }
                st.granules[*gi].region_bound[2] = st.granules[*gi].region_bound[1];
                // context setting
                s_r0 = 0; // s_r1 = 0;
            }
            // --- region tables (region0/1/2_table, select_htabB) ---
            // region 0 table
            shift_s2(&mod_t0, gg_ctx[i] as i32, t_r0);
            t_r0 = dec_s(dec, &mod_t0);
            st.granules[*gi].region_table[0] = t_r0 as i8;
            // region 1 table
            shift_s2(&mod_t1, t_r0, s_r0);
            t_r1 = dec_s(dec, &mod_t1);
            st.granules[*gi].region_table[1] = t_r1 as i8;
            // region 2 table
            if st.granules[*gi].window_switching == 0 {
                shift_s2(&mod_t2, t_r0, t_r1);
                t_r2 = dec_s(dec, &mod_t2);
                st.granules[*gi].region_table[2] = t_r2 as i8;
            } else {
                st.granules[*gi].region_table[2] = 0;
            }
            // small values table
            shift_b(&mod_ts, ctx_sv);
            t_sv = dec_b(dec, &mod_ts);
            st.granules[*gi].select_htabb = t_sv as i8;
            ctx_sv = ((ctx_sv << 1) | t_sv) & 0xF;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// scalefactor sharing (2/4 bit)
// ---------------------------------------------------------------------------

/// encode scalefactor sharing (2/4 bit)
pub fn pmp_encode_sharing(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut model = mod_s(16, 16, 3);

    // encoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush model
        model.flush_model(1);
        let chain = granule_chain(st, ch);
        let mut c = 0i32;
        // encode one channel (only every second granule matters)
        let mut i = 0;
        while i + 1 < chain.len() {
            let sl0 = st.granules[chain[i]].slength as i32;
            let sl1 = st.granules[chain[i + 1]].slength as i32;
            model.shift_context(c);
            model.shift_context(sl0);
            model.shift_context(sl1);
            c = st.granules[chain[i]].share as i32;
            encode_ari_s(enc, &mut model, c);
            i += 2;
        }
    }

    true
}

/// decode scalefactor sharing (2/4 bit)
pub fn pmp_decode_sharing(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut model = mod_s(16, 16, 3);

    // decoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush model
        model.flush_model(1);
        let chain = granule_chain(st, ch);
        let mut c = 0i32;
        // decode one channel (only every second granule matters)
        let mut i = 0;
        while i + 1 < chain.len() {
            let sl0 = st.granules[chain[i]].slength as i32;
            let sl1 = st.granules[chain[i + 1]].slength as i32;
            model.shift_context(c);
            model.shift_context(sl0);
            model.shift_context(sl1);
            let v = decode_ari_s(dec, &mut model);
            st.granules[chain[i]].share = v as i8;
            c = v;
            i += 2;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// preemphasis setting (1 bit)
// ---------------------------------------------------------------------------

/// encode the preemphasis setting (1 bit)
pub fn pmp_encode_preemphasis(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut model = mod_b(16, 1);

    // encoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush model
        model.flush_model(1);
        let chain = granule_chain(st, ch);
        let mut ctx: i32 = 0;
        // encode one channel
        for gi in &chain {
            model.shift_context(ctx);
            let c = st.granules[*gi].preemphasis as i32;
            encode_ari_b(enc, &mut model, c);
            ctx = ((ctx << 1) | c) & 0xF;
        }
    }

    true
}

/// decode the preemphasis setting (1 bit)
pub fn pmp_decode_preemphasis(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut model = mod_b(16, 1);

    // decoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush model
        model.flush_model(1);
        let chain = granule_chain(st, ch);
        let mut ctx: i32 = 0;
        // decode one channel
        for gi in &chain {
            model.shift_context(ctx);
            let c = decode_ari_b(dec, &mut model);
            st.granules[*gi].preemphasis = c as i8;
            ctx = ((ctx << 1) | c) & 0xF;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// coarse sf setting (1 bit)
// ---------------------------------------------------------------------------

/// encode the coarse sf setting (1 bit)
pub fn pmp_encode_coarse_sf(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut model = mod_b(16, 1);

    // encoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush model
        model.flush_model(1);
        let chain = granule_chain(st, ch);
        let mut ctx: i32 = 0;
        // encode one channel
        for gi in &chain {
            model.shift_context(ctx);
            let c = st.granules[*gi].coarse_scalefactors as i32;
            encode_ari_b(enc, &mut model, c);
            ctx = ((ctx << 1) | c) & 0xF;
        }
    }

    true
}

/// decode the coarse sf setting (1 bit)
pub fn pmp_decode_coarse_sf(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut model = mod_b(16, 1);

    // decoding start
    for ch in 0..st.g_nchannels as usize {
        // reset and flush model
        model.flush_model(1);
        let chain = granule_chain(st, ch);
        let mut ctx: i32 = 0;
        // decode one channel
        for gi in &chain {
            model.shift_context(ctx);
            let c = decode_ari_b(dec, &mut model);
            st.granules[*gi].coarse_scalefactors = c as i8;
            ctx = ((ctx << 1) | c) & 0xF;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// subblock gain (9 bit)
// ---------------------------------------------------------------------------

/// encode the subblock gain (9 bit)
pub fn pmp_encode_subblock_gain(st: &PackState, enc: &mut Aricoder) -> bool {
    let mut model = mod_s(8, 8, 1);

    // encoding start
    model.shift_context(0);
    for ch in 0..st.g_nchannels as usize {
        let chain = granule_chain(st, ch);
        // encode one channel
        for gi in &chain {
            if st.granules[*gi].window_switching != 0 {
                for sb in 0..3 {
                    let v = st.granules[*gi].sb_gain[sb] as i32;
                    encode_ari_s(enc, &mut model, v);
                    model.shift_context(v);
                }
            }
        }
    }

    true
}

/// decode the subblock gain (9 bit)
pub fn pmp_decode_subblock_gain(st: &mut PackState, dec: &mut Aricoder) -> bool {
    let mut model = mod_s(8, 8, 1);

    // decoding start
    model.shift_context(0);
    for ch in 0..st.g_nchannels as usize {
        let chain = granule_chain(st, ch);
        // decode one channel
        for gi in &chain {
            if st.granules[*gi].window_switching != 0 {
                for sb in 0..3 {
                    let v = decode_ari_s(dec, &mut model);
                    st.granules[*gi].sb_gain[sb] = v as i8;
                    model.shift_context(v);
                }
            } else {
                st.granules[*gi].sb_gain = [0; 3];
            }
        }
    }

    true
}

// ---------------------------------------------------------------------------
// unmute data storage (used with muted frames)
// ---------------------------------------------------------------------------

/// store unmute data
pub fn pmp_store_unmute_data(st: &PackState, str_out: &mut IoStream) -> bool {
    // as simple as it gets...
    str_out.write(&st.unmute_data, 1, st.unmute_data_size);
    true
}

/// unstore unmute data
pub fn pmp_unstore_unmute_data(st: &mut PackState, str_in: &mut IoStream) -> bool {
    // read # of muted frames, set data size
    let mut n = [0u8; 1];
    str_in.read(&mut n, 1, 1);
    if str_in.chkeof() {
        n[0] = 0;
    }
    let n = n[0];
    st.unmute_data_size = 1 + (n as i32) * crate::state::unmute_record_size(st.g_nchannels);

    // alloc memory for unmute data
    st.unmute_data = vec![0u8; st.unmute_data_size as usize];
    st.unmute_data[0] = n;

    // read unmute data
    let rest = (st.unmute_data_size - 1) as usize;
    str_in.read(&mut st.unmute_data[1..1 + rest], 1, st.unmute_data_size - 1);

    // check for eof
    if str_in.chkeof() {
        st.errormessage = "unexpected end of data".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }

    true
}

// ---------------------------------------------------------------------------
// universal context from global gain
// ---------------------------------------------------------------------------

/// build universal context from global gain
pub fn pmp_build_context(st: &mut PackState) -> bool {
    st.gg_context[0] = None;
    st.gg_context[1] = None;

    // number of granules (per channel)
    let ngr = (st.g_nframes * 2) as usize;

    // alloc memory for both global gain contexts
    st.gg_context[0] = Some(vec![0u8; ngr]);
    st.gg_context[1] = Some(vec![0u8; ngr]);

    // count channel 0 symbol occurences
    let mut count_gg = [[0i32; 256]; 2];
    let chain0 = granule_chain(st, 0);
    for (gg0, gi) in chain0.iter().enumerate() {
        let v = (st.granules[*gi].global_gain as i32) & 0xFF;
        st.gg_context[0].as_mut().unwrap()[gg0] = v as u8;
        count_gg[0][v as usize] += 1;
    }
    for i in 1..256 {
        count_gg[0][i] += count_gg[0][i - 1];
    }

    // count channel 1 diff symbol occurences
    if st.g_nchannels == 2 {
        let chain1 = granule_chain(st, 1);
        for (gg1, gi) in chain1.iter().enumerate() {
            let v = (st.granules[*gi].global_gain as i32) & 0xFF;
            st.gg_context[1].as_mut().unwrap()[gg1] = v as u8;
            count_gg[1][v as usize] += 1;
        }
        for i in 1..256 {
            count_gg[1][i] += count_gg[1][i - 1];
        }
    }

    // analyse and process gg_context
    for (ch, cnt) in count_gg.iter().enumerate().take(st.g_nchannels as usize) {
        // find the interval with the highest increase
        let mut lbound = 0i32;
        let mut inc0 = 0i32;
        for i in 0..(256 - GG_CONTEXT_SIZE) as usize {
            let inc1 = cnt[i + GG_CONTEXT_SIZE as usize] - cnt[i];
            if inc1 >= inc0 {
                inc0 = inc1;
                lbound = i as i32;
            }
        }
        // process gg_context
        for gg in st.gg_context[ch].as_mut().unwrap().iter_mut() {
            if (*gg as i32) >= lbound {
                *gg = (*gg).wrapping_sub(lbound as u8);
                if (*gg as i32) >= GG_CONTEXT_SIZE {
                    *gg = (GG_CONTEXT_SIZE - 1) as u8;
                }
            } else {
                *gg = 0;
            }
        }
    }

    true
}

// ---------------------------------------------------------------------------
// LAME ancillary data predictor
// ---------------------------------------------------------------------------

/// predict LAME ancillary data.
/// returns true if the prediction matched `ref_data` (C++ returned NULL).
/// With `ref_data == None` the function only (re)builds the prediction.
pub fn pmp_predict_lame_anc(
    anc: &mut LameAncPredictor,
    mut nbits: i32,
    ref_data: Option<&[u8]>,
) -> bool {
    const B01: u8 = 0x55;
    const B10: u8 = 0xAA;
    const B00: u8 = 0x00;
    const B11: u8 = 0xFF;

    if nbits < 0 {
        // (re-)init prediction
        anc.lame_bit = 0;
        anc.lame_str_len = 4;
        anc.lame_str[0..4].copy_from_slice(b"LAME");
        anc.alt_pred = false;
    } else {
        let offset = nbits % 8;
        let nbytes = nbits / 8;
        // build prediction
        if !anc.alt_pred {
            // option 0 - lame predictor
            let mut i = 0usize;
            while nbits >= 8 && i < 4 {
                anc.pred[i] = anc.lame_str[i]; // 'LAME'
                nbits -= 8;
                i += 1;
            }
            if nbits >= 32 {
                while nbits >= 8 && i < anc.lame_str_len as usize {
                    anc.pred[i] = anc.lame_str[i]; // version number
                    nbits -= 8;
                    i += 1;
                }
            }
            while nbits > 0 {
                anc.pred[i] = if anc.lame_bit == 0 { B01 } else { B10 }; // switching bits
                nbits -= 8;
                i += 1;
            }
            anc.lame_bit ^= offset % 2;
        }
        // else: keep the prediction as is
        if let Some(reff) = ref_data {
            // compare prediction with reference
            // (C++ promotes the u8 to int before shifting; `>> 8` yields 0)
            if (nbytes == 0 || anc.pred[..nbytes as usize] == reff[..nbytes as usize])
                && reff[nbytes as usize] as i32
                    == ((anc.pred[nbytes as usize] as i32) >> (8 - offset))
            {
                // match
                return true;
            }
            // no match - check alternatives / relearn version number
            if nbytes >= 8 && reff[..4] == anc.lame_str[..4] {
                // 'LAME' match, relearn version number (if any)
                let mut i = 4usize;
                while i < 4 + 16 && i < nbytes as usize {
                    if reff[i] == B01 {
                        anc.lame_bit = 0;
                        break;
                    } else if reff[i] == B10 {
                        anc.lame_bit = 1;
                        break;
                    }
                    anc.lame_str[i] = reff[i];
                    i += 1;
                }
                anc.lame_str_len = i as i32;
            } else if nbytes == 0 && offset > 1 {
                if (B00 ^ reff[0]) >> (8 - offset) == 0 {
                    anc.pred.fill(B00);
                    anc.alt_pred = true;
                } else if (B11 ^ reff[0]) >> (8 - offset) == 0 {
                    anc.pred.fill(B11);
                    anc.alt_pred = true;
                } else {
                    anc.alt_pred = false;
                }
            } else if nbytes >= 1 {
                if B00 == reff[0] {
                    anc.pred.fill(B00);
                    anc.alt_pred = true;
                } else if B11 == reff[0] {
                    anc.pred.fill(B11);
                    anc.alt_pred = true;
                } else {
                    anc.alt_pred = false;
                }
            }
        }
        // else: no reference, so prediction is assumed correct
    }

    false
}

// ---------------------------------------------------------------------------
// main compress/decompress driver functions
// ---------------------------------------------------------------------------

/// compress currently read MP3 structure to the output stream
pub fn compress_mp3(st: &mut PackState, str_out: &mut IoStream) -> bool {
    // --- check for incompatibilities and problems ---

    // unsupported stuff -> not recoverable
    if st.ai.i_protection == -1 {
        st.errormessage = "inconsistent use of checksums, not supported".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }
    if st.ai.i_stereo_int == -1 {
        st.errormessage = "inconsistent use of int stereo, not supported".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }
    if st.ai.i_emphasis == -1 {
        st.errormessage = "inconsistent use of emphasis, not supported".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }
    if st.ai.i_mixed != 0x0 {
        st.errormessage = "mixed blocks used, not supported".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }

    // inconsistencies -> recoverable
    if st.ai.i_privbit == -1 {
        st.errormessage = "inconsistent private bit".to_string();
        st.errorlevel = EL_WARNING;
        st.ai.i_privbit = 0;
    }
    if st.ai.i_copyright == -1 {
        st.errormessage = "inconsistent copyright bit".to_string();
        st.errorlevel = EL_WARNING;
        st.ai.i_copyright = 0;
    }
    if st.ai.i_original == -1 {
        st.errormessage = "inconsistent original bit".to_string();
        st.errorlevel = EL_WARNING;
        st.ai.i_original = 1;
    }
    if st.ai.i_padbits != 0 {
        st.errormessage = "non-zero padbits found".to_string();
        st.errorlevel = EL_WARNING;
        st.ai.i_padbits = 0;
    }

    // --- write PMP header with some basic info ---

    // PMP magic number & version byte
    str_out.write(&PMP_MAGIC, 1, 2);
    str_out.write(&[APP_VERSION], 1, 1);

    // PMP header data
    if !pmp_write_header(st, str_out) {
        return false;
    }

    // --- mute frames, store fix data (only if broken) ---
    if st.n_bad_first > 0 {
        // mute all frames up to last bad
        let chainf = frame_chain(st);
        let mut i = 0;
        while st.n_bad_first > 0 && i < chainf.len() {
            crate::mp3::mp3_mute_frame(st, chainf[i]);
            st.n_bad_first -= 1;
            i += 1;
        }
        // store fix data
        if !pmp_store_unmute_data(st, str_out) {
            return false;
        }
    }

    // --- actual compressed data writing starts here ---

    // init arithmetic compression
    let mut encoder = Aricoder::new(str_out, MODE_WRITE);

    // id3 tags / other tags / garbage
    if ((st.data_before_size > 0) || (st.data_after_size > 0)) && !pmp_encode_id3(st, &mut encoder) {
        return false;
    }

    // frame header data
    if st.ai.i_padding != 0 {
        // padding bits
        if !pmp_encode_padding(st, &mut encoder) {
            return false;
        }
    }
    if st.ai.i_sblocks != 0 {
        // special block types
        if !pmp_encode_block_types(st, &mut encoder) {
            return false;
        }
    }
    // global gain
    if !pmp_encode_global_gain(st, &mut encoder) {
        return false;
    }
    // build context from global gain
    if !pmp_build_context(st) {
        return false;
    }
    // slength
    if !pmp_encode_slength(st, &mut encoder) {
        return false;
    }
    if !pmp_encode_region_data(st, &mut encoder) {
        return false;
    }
    if st.ai.i_share != 0 {
        // scalefactor sharing
        if !pmp_encode_sharing(st, &mut encoder) {
            return false;
        }
    }
    if st.ai.i_preemphasis != 0 {
        // preemphasis
        if !pmp_encode_preemphasis(st, &mut encoder) {
            return false;
        }
    }
    if st.ai.i_coarse != 0 {
        // coarse setting
        if !pmp_encode_coarse_sf(st, &mut encoder) {
            return false;
        }
    }
    if st.ai.i_sbgain != 0 {
        // subblock gain
        if !pmp_encode_subblock_gain(st, &mut encoder) {
            return false;
        }
    }
    if st.ai.i_stereo_ms != 0 {
        // ms stereo settings
        if !pmp_encode_stereo_ms(st, &mut encoder) {
            return false;
        }
    }
    // main data
    if let Err(e) = pmp_encode_main_data(st, &mut encoder) {
        st.errormessage = e;
        st.errorlevel = EL_FATAL;
        return false;
    }

    // finalize arithmetic compression
    encoder.flush();

    // --- final checks ---

    // errormessage if write error
    if str_out.chkerr() {
        st.errormessage = "write error, possibly drive is full".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }

    // get filesize
    st.pmpfilesize = str_out.getsize() as i32;

    true
}

/// decompress PMP stream into the frame structure
pub fn uncompress_pmp(st: &mut PackState, str_in: &mut IoStream) -> bool {
    // --- read PMP header and analyse basic info ---

    // PMP magic number (skip 2 bytes, no need to check again)
    let mut hcode = [0u8; 1];
    str_in.read(&mut hcode, 1, 1);
    str_in.read(&mut hcode, 1, 1);

    // version number
    str_in.read(&mut hcode, 1, 1);
    if hcode[0] != APP_VERSION {
        st.errormessage = format!(
            "incompatible file, use {} v{}.{}",
            APP_NAME,
            hcode[0] / 10,
            hcode[0] % 10
        );
        st.errorlevel = EL_FATAL;
        return false;
    }

    // read and analyse header
    if !pmp_read_header(st, str_in) {
        return false;
    }

    // --- unstore unmute fix data for later use (only if needed) ---
    if st.n_bad_first > 0 {
        if !pmp_unstore_unmute_data(st, str_in) {
            return false;
        }
        st.n_bad_first = 0; // not a beautiful solution
    }

    // --- actual decompression starts here ---

    // init arithmetic decompression
    let mut decoder = Aricoder::new(str_in, MODE_READ);

    // id3 tags / other tags / garbage
    if ((st.data_before_size > 0) || (st.data_after_size > 0)) && !pmp_decode_id3(st, &mut decoder) {
        return false;
    }

    // frame header data
    if st.ai.i_padding != 0 {
        // padding bits
        if !pmp_decode_padding(st, &mut decoder) {
            return false;
        }
    }
    if st.ai.i_sblocks != 0 {
        // special block types
        if !pmp_decode_block_types(st, &mut decoder) {
            return false;
        }
    }
    // global gain
    if !pmp_decode_global_gain(st, &mut decoder) {
        return false;
    }
    // build context from global gain
    if !pmp_build_context(st) {
        return false;
    }
    // slength
    if !pmp_decode_slength(st, &mut decoder) {
        return false;
    }
    if !pmp_decode_region_data(st, &mut decoder) {
        return false;
    }
    if st.ai.i_share != 0 {
        // scalefactor sharing
        if !pmp_decode_sharing(st, &mut decoder) {
            return false;
        }
    }
    if st.ai.i_preemphasis != 0 {
        // preemphasis
        if !pmp_decode_preemphasis(st, &mut decoder) {
            return false;
        }
    }
    if st.ai.i_coarse != 0 {
        // coarse setting
        if !pmp_decode_coarse_sf(st, &mut decoder) {
            return false;
        }
    }
    if st.ai.i_sbgain != 0 {
        // subblock gain
        if !pmp_decode_subblock_gain(st, &mut decoder) {
            return false;
        }
    }
    if st.ai.i_stereo_ms != 0 {
        // ms stereo settings
        if !pmp_decode_stereo_ms(st, &mut decoder) {
            return false;
        }
    }
    // main data
    if let Err(e) = pmp_decode_main_data(st, &mut decoder) {
        st.errormessage = e;
        st.errorlevel = EL_FATAL;
        return false;
    }
    // finalize arithmetic decompression (dropped, no flush needed)

    // --- unmute frames and reverse to bad (but bitwise identical) state ---
    if st.unmute_data_size > 0 {
        // unmute all frames up to # found in unmute data first byte
        let chainf = frame_chain(st);
        let mut i = 0;
        while (st.n_bad_first as u8) < st.unmute_data[0] && i < chainf.len() {
            crate::mp3::mp3_unmute_frame(st, chainf[i]);
            st.n_bad_first += 1;
            i += 1;
        }
    }

    // --- final checks ---

    // get filesize
    st.pmpfilesize = str_in.getsize() as i32;

    true
}

// ---------------------------------------------------------------------------
// main data (the big one)
// ---------------------------------------------------------------------------

/// read the value `arr[i]` from the long or short context array (mirrors the
/// C++ `ctx_h_abs` / `ctx_h_sgn` / `ctx_h_len` pointer selection).
#[inline]
fn ctx_h_val(a_l: &[Vec<u8>; 2], a_s: &[Vec<u8>; 2], sbl: i32, ch: usize, idx: usize) -> u8 {
    if sbl == 0 {
        a_l[ch][idx]
    } else {
        a_s[ch][idx]
    }
}

/// encode the main data (scalefactors & coefficients + stuffing bits +
/// bitrate + aux data size & padding/aux bits) into the arithmetic coder.
pub fn pmp_encode_main_data(st: &mut PackState, enc: &mut Aricoder) -> Result<(), String> {
    let nch = st.g_nchannels as usize;

    // chain indices (list frames / granules in linked-list order)
    let chainf = frame_chain(st);
    let lastframe = st.lastframe;

    let i_samplerate = st.ai.i_samplerate as i32;
    let i_bitrate = st.ai.i_bitrate as i32;
    let i_bit_res = st.ai.i_bit_res as i32;
    let i_channels = st.ai.i_channels as i32;
    let j_coding = i_channels == MP3_STEREO || i_channels == MP3_JOINT_STEREO;
    let bitrate_pred: &[i32; 2049] = &MP3_BITRATE_PRED[i_samplerate as usize];

    // statistical models (mirror C++ INIT_MODEL_* macros with c_lim = 511)
    let mod_svb = wrap_s(mod_s(576 / 4 + 1 + 1, 576 / 4 + 1 + 1, 1));
    let mod_bvf = wrap_s(mod_s(576 / 2 + 1, 0, 0));
    let mod_nst = wrap_s(mod_s(STUFFING_STEP + 1, STUFFING_STEP + 1, 1));
    let mod_bst = wrap_b(mod_b(16, 1));
    let mod_pad = wrap_b(mod_b(256, 1));
    let mod_pap = wrap_b(mod_b(0, 0));
    let mod_aux = wrap_s(mod_s(AUX_DATA_STEP + 1, AUX_DATA_STEP + 1, 1));
    let mod_btr = wrap_s(mod_s(16, 16, 1));
    let mod_res = wrap_b(mod_b(13 + 1, 2));

    let mut mod_scf: [[[Option<Rc<RefCell<ModelS>>>; 10]; 4]; 2] = Default::default();
    let mut mod_abv: [[[Option<Rc<RefCell<ModelS>>>; 32]; 8]; 2] = Default::default();
    let mut mod_asv: [[[Option<Rc<RefCell<ModelB>>>; 2]; 8]; 2] = Default::default();
    let mut mod_sgn: [[Option<Rc<RefCell<ModelB>>>; 8]; 2] = Default::default();
    let mut mod_len: [[Option<Rc<RefCell<ModelS>>>; 14]; 2] = Default::default();

    const ABV_MAXS: [i32; 16] = [0, 1, 2, 2, 0, 3, 3, 5, 5, 5, 7, 7, 7, 15, 0, 15];
    for ch in 0..2usize {
        let ms = [2i32, 4, 8, 16];
        for (sl, scf_slots) in mod_scf[ch].iter_mut().enumerate() {
            for slot in scf_slots.iter_mut() {
                *slot = Some(wrap_s(mod_s(ms[sl], 16, 2)));
            }
        }
        let nflags = if j_coding && ch > 0 { 8 } else { 2 };
        for f in 0..nflags {
            mod_abv[ch][f][0] = None;
            for i in 1..16usize {
                if i == 4 || i == 14 {
                    mod_abv[ch][f][i] = None;
                } else {
                    mod_abv[ch][f][i] = Some(wrap_s(mod_s(ABV_MAXS[i] + 1, 16, 2)));
                }
            }
            let m16 = wrap_s(mod_s(15 + 1, 16, 2));
            for slot in mod_abv[ch][f][16..=23].iter_mut() {
                *slot = Some(Rc::clone(&m16));
            }
            let m24 = wrap_s(mod_s(15 + 1, 16, 2));
            for slot in mod_abv[ch][f][24..=31].iter_mut() {
                *slot = Some(Rc::clone(&m24));
            }
            mod_asv[ch][f][0] = Some(wrap_b(mod_b(16, 2)));
            mod_asv[ch][f][1] = Some(wrap_b(mod_b(16, 2)));
            mod_sgn[ch][f] = Some(wrap_b(mod_b(16, 2)));
        }
        for (i, slot) in mod_len[ch].iter_mut().enumerate().skip(1) {
            *slot = Some(wrap_s(mod_s(i as i32 + 1, 13 + 1, 1)));
        }
    }

    // local coefficient / scalefactor / context storage
    let mut scf_c: [Vec<u8>; 2] = [vec![0; 21], vec![0; 21]];
    let mut scf_l_long: [Vec<u8>; 2] = [vec![0; 21], vec![0; 21]];
    let mut scf_l_short: [Vec<u8>; 2] = [vec![0; 21], vec![0; 21]];
    let mut abs_c: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut sgn_c: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut len_c: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut lbt_c: [Vec<u32>; 2] = [vec![0; 576], vec![0; 576]];
    let mut absl_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut abss_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut sgnl_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut sgns_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut lenl_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut lens_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    // huffman reader for the original MP3 main data
    let mut huffman = HuffmanReader::new(&st.main_data[..st.main_data_size as usize]);

    // reset ancillary predictor
    pmp_predict_lame_anc(&mut st.anc, -1, None);

    let frames = &st.frames;
    let granules = &mut st.granules;
    let anc = &mut st.anc;

    let mut bitp: i32;
    let mut flags = 0i32;
    let mut shared = [[0i8; 4]; 2];
    let mut ctx_svb = [0u8; 2];
    let mut ctx_nst = 0u8;
    let mut ctx_bst = 0u8;
    let mut ctx_aux = 0u8;
    let mut ctx_pad: u8;
    let mut pad_and_aux = vec![0u8; 2048];

    for &fi in &chainf {
        let fr = &frames[fi];
        let frame_n = fr.n;
        let is_lastframe = Some(fi) == lastframe;
        bitp = 0;
        for gr in 0..2i32 {
            for ch in 0..nch as i32 {
                let ch_u = ch as usize;
                let gi = fr.granules[ch_u][gr as usize];
                let granule = &mut granules[gi];

                // --- MAIN DATA DECODING: PREPARATIONS ---
                let sbl = if granule.block_type as i32 == SHORT_BLOCK { 1 } else { 0 };
                let lmaxp = granule.main_data_bit as i32;
                let slen = SLENGTH_TABLE[granule.slength as usize];
                let mut rb = granule.region_bound;
                let rt = granule.region_table;
                let select_htabb = granule.select_htabb as i32;
                // set compression flags
                flags = if !j_coding || ch == 0 {
                    sbl
                } else {
                    sbl | ((fr.stereo_ms as i32) << 1) | ((flags ^ sbl) << 2)
                };
                // store sharing info
                if gr == 0 {
                    shared[ch_u][0] = (granule.share >> 3) & 0x1;
                    shared[ch_u][1] = (granule.share >> 2) & 0x1;
                    shared[ch_u][2] = (granule.share >> 1) & 0x1;
                    shared[ch_u][3] = granule.share & 0x1;
                }
                // reset counter
                huffman.reset_counter();
                let mut bad_coding = false;

                // ---> SCALEFACTOR PROCESSING <---
                if sbl == 0 {
                    // long blocks
                    let mut ctx_scf = 0u8;
                    let mut p = 0i32;
                    for g in 0..4i32 {
                        let sl = slen[if g < 2 { 0 } else { 1 }];
                        if (gr != 0) && (shared[ch_u][g as usize] != 0) {
                            // shared
                            for k in 0..SCF_WIDTH[g as usize] as usize {
                                scf_c[ch_u][p as usize + k] = scf_l_long[ch_u][p as usize + k];
                            }
                            p = SCF_BOUNDS[g as usize];
                            ctx_scf = scf_c[ch_u][(p - 1) as usize];
                        } else if sl == 0 {
                            // zero slength
                            for k in 0..SCF_WIDTH[g as usize] as usize {
                                scf_c[ch_u][p as usize + k] = 0;
                            }
                            p = SCF_BOUNDS[g as usize];
                            ctx_scf = 0;
                        } else {
                            // non shared
                            let sfc = mod_scf[ch_u][(sl - 1) as usize]
                                [if shared[ch_u][g as usize] != 0 { (g | 4) as usize } else { g as usize }]
                                .as_ref()
                                .unwrap();
                            while p < SCF_BOUNDS[g as usize] {
                                let v = huffman.read_bits(sl) as u8;
                                scf_c[ch_u][p as usize] = v;
                                shift_s2(sfc, ctx_scf as i32, scf_l_long[ch_u][p as usize] as i32);
                                enc_s(enc, sfc, v as i32);
                                ctx_scf = v;
                                p += 1;
                            }
                        }
                    }
                    std::mem::swap(&mut scf_c[ch_u], &mut scf_l_long[ch_u]);
                } else {
                    // short blocks
                    for _i in 0..3i32 {
                        let mut ctx_scf = 0u8;
                        let mut p = 0i32;
                        for g in 0..2i32 {
                            let sl = slen[g as usize];
                            if sl == 0 {
                                for k in 0..SCF_LH_WIDTH_SHORT[g as usize] as usize {
                                    scf_c[ch_u][p as usize + k] = 0;
                                }
                                p = SCF_LH_BOUNDS_SHORT[g as usize];
                            } else {
                                let sfc = mod_scf[ch_u][(sl - 1) as usize][(g as usize) | 0x8]
                                    .as_ref()
                                    .unwrap();
                                while p < SCF_LH_BOUNDS_SHORT[g as usize] {
                                    let v = huffman.read_bits(sl) as u8;
                                    scf_c[ch_u][p as usize] = v;
                                    shift_s2(sfc, ctx_scf as i32, scf_l_short[ch_u][p as usize] as i32);
                                    enc_s(enc, sfc, v as i32);
                                    ctx_scf = v;
                                    p += 1;
                                }
                            }
                        }
                        std::mem::swap(&mut scf_c[ch_u], &mut scf_l_short[ch_u]);
                    }
                }

                // sanity check
                if huffman.get_count() > lmaxp {
                    return Err(format!("huffman decoding error (in frame #{})", frame_n));
                }

                // ---> COEFFICIENTS PROCESSING <---
                // --- COEFFICIENT DECODING: BIG VALUES ---
                let mut p = 0i32;
                let mut bitc;
                let mut r = 0i32;
                while r < 3 && !bad_coding {
                    if rt[r as usize] == 0 {
                        p = rb[r as usize] as i32;
                        r += 1;
                        continue;
                    }
                    let bv_table = &BV_DEC_TABLE[rt[r as usize] as usize];
                    if bv_table.h.is_none() {
                        return Err(format!(
                            "bad huffman table ({}) used (in frame #{})",
                            rt[r as usize], frame_n
                        ));
                    }
                    let conv_set = bv_table.h.unwrap();
                    let linbits = bv_table.linbits as i32;
                    while p < rb[r as usize] as i32 {
                        bitc = huffman.get_count();
                        let mut pair = [0u8; 2];
                        huffman.decode_pair(conv_set, &mut pair);
                        abs_c[ch_u][(p + 1) as usize] = pair[0];
                        abs_c[ch_u][(p + 2) as usize] = pair[1];
                        for _ in 0..2 {
                            if abs_c[ch_u][(p + 1) as usize] > 0 {
                                if linbits > 0 && abs_c[ch_u][(p + 1) as usize] == 15 {
                                    let lbt = huffman.read_bits(linbits);
                                    lbt_c[ch_u][p as usize] = lbt;
                                    len_c[ch_u][(p + 1) as usize] = bitlen8192p(lbt as i32);
                                }
                                sgn_c[ch_u][(p + 1) as usize] = huffman.read_bit();
                            }
                            p += 1;
                        }
                        if huffman.get_count() > lmaxp {
                            // bad coding rollback
                            bad_coding = true;
                            p -= 1;
                            abs_c[ch_u][(p + 1) as usize] = 0;
                            sgn_c[ch_u][(p + 1) as usize] = 0;
                            len_c[ch_u][(p + 1) as usize] = 0;
                            p -= 1;
                            abs_c[ch_u][(p + 1) as usize] = 0;
                            sgn_c[ch_u][(p + 1) as usize] = 0;
                            len_c[ch_u][(p + 1) as usize] = 0;
                            huffman.rewind_bits(huffman.get_count() - bitc);
                            break;
                        }
                    }
                    r += 1;
                }

                // --- COEFFICIENT DECODING: SMALL VALUES ---
                if !bad_coding {
                    let conv = if select_htabb != 0 { &HTABB_DEC } else { &HTABA_DEC };
                    bitc = huffman.get_count();
                    while p < 576 {
                        if bitc == lmaxp {
                            break;
                        }
                        let mut quad = [0u8; 4];
                        huffman.decode_quadruple(conv, &mut quad);
                        let base = (p + 1) as usize;
                        abs_c[ch_u][base..base + 4].copy_from_slice(&quad);
                        for _ in 0..4 {
                            if abs_c[ch_u][(p + 1) as usize] > 0 {
                                sgn_c[ch_u][(p + 1) as usize] = huffman.read_bit();
                            }
                            p += 1;
                        }
                        if huffman.get_count() > lmaxp {
                            // bad coding rollback
                            // (C++ `bad_coding = true;` is never read after this point)
                            for _ in 0..4 {
                                p -= 1;
                                abs_c[ch_u][(p + 1) as usize] = 0;
                                sgn_c[ch_u][(p + 1) as usize] = 0;
                            }
                            huffman.rewind_bits(huffman.get_count() - bitc);
                            break;
                        }
                        bitc = huffman.get_count();
                    }
                }

                // --- SETTING AND ENCODING OF SV BOUND ---
                let sv_bound = p;
                if sbl == 0 {
                    shift_s(&mod_svb, ctx_svb[ch_u] as i32);
                } else {
                    shift_s(&mod_svb, 144 + 1);
                }
                if p >= rb[2] as i32 {
                    // for proper files: encode actual sv bound
                    let c = sv_bound >> 2;
                    enc_s(enc, &mod_svb, c);
                    if sbl == 0 {
                        ctx_svb[ch_u] = c as u8;
                    }
                } else {
                    // for broken files: encode negative diff with r2 bound
                    enc_s(enc, &mod_svb, 144 + 1);
                    let c = (rb[2] as i32 - sv_bound) >> 1;
                    enc_s(enc, &mod_bvf, c);
                    // fix bounds
                    rb[2] = p as i16;
                    for i in (0..2).rev() {
                        if p < rb[i] as i32 {
                            rb[i] = p as i16;
                        } else {
                            break;
                        }
                    }
                }

                // --- COEFFICIENT ENCODING: PREPARATIONS ---
                let mut ctx_abs = 0i32;
                let mut ctx_pat = 0u8;
                let rlb = rb[2] as i32;
                let mod_sgc = mod_sgn[ch_u][flags as usize].as_ref().unwrap();

                // --- COEFFICIENT ENCODING: SMALL VALUES ---
                let mod_asc = mod_asv[ch_u][flags as usize][select_htabb as usize]
                    .as_ref()
                    .unwrap();
                let mut qp = sv_bound - 1;
                while qp >= rlb {
                    let idx = (qp + 1) as usize;
                    let av = abs_c[ch_u][idx];
                    shift_b2(
                        mod_asc,
                        ctx_pat as i32,
                        ctx_h_val(&absl_ctx_h, &abss_ctx_h, sbl, ch_u, idx) as i32,
                    );
                    enc_b(enc, mod_asc, av as i32);
                    ctx_pat = ((ctx_pat << 1) | av) & 0xF;
                    ctx_abs = (2 * av as i32 + ctx_abs + 2) / 3;
                    if av == 1 {
                        shift_b2(
                            mod_sgc,
                            ctx_h_val(&absl_ctx_h, &abss_ctx_h, sbl, ch_u, idx) as i32,
                            ctx_h_val(&sgnl_ctx_h, &sgns_ctx_h, sbl, ch_u, idx) as i32,
                        );
                        enc_b(enc, mod_sgc, sgn_c[ch_u][idx] as i32);
                    }
                    qp -= 1;
                }

                // --- COEFFICIENT ENCODING: BIG VALUES ---
                let mut r = 2i32;
                while r >= 0 {
                    let rlb = if r == 0 { 0 } else { rb[(r - 1) as usize] as i32 };
                    if qp < rlb {
                        r -= 1;
                        continue;
                    }
                    if rt[r as usize] == 0 {
                        for k in 0..(qp - rlb) as usize {
                            abs_c[ch_u][rlb as usize + 1 + k] = 0;
                        }
                        qp = rlb;
                        ctx_abs = 0;
                        r -= 1;
                        continue;
                    }
                    // set table and linbits
                    let bv_table = &BV_DEC_TABLE[rt[r as usize] as usize];
                    let linbits = bv_table.linbits as i32;
                    let mod_abc = mod_abv[ch_u][flags as usize][rt[r as usize] as usize]
                        .as_ref()
                        .unwrap();
                    let mod_lnc = mod_len[ch_u][linbits as usize].as_ref();
                    while qp >= rlb {
                        let idx = (qp + 1) as usize;
                        let av = abs_c[ch_u][idx];
                        shift_s2(
                            mod_abc,
                            ctx_abs,
                            ctx_h_val(&absl_ctx_h, &abss_ctx_h, sbl, ch_u, idx) as i32,
                        );
                        enc_s(enc, mod_abc, av as i32);
                        ctx_abs = (2 * av as i32 + ctx_abs + 2) / 3;
                        if av > 0 {
                            shift_b2(
                                mod_sgc,
                                ctx_h_val(&absl_ctx_h, &abss_ctx_h, sbl, ch_u, idx) as i32,
                                ctx_h_val(&sgnl_ctx_h, &sgns_ctx_h, sbl, ch_u, idx) as i32,
                            );
                            enc_b(enc, mod_sgc, sgn_c[ch_u][idx] as i32);
                            if linbits > 0 && av == 15 {
                                // loop invariant condition
                                let lnc = mod_lnc.unwrap();
                                shift_s(
                                    lnc,
                                    ctx_h_val(&lenl_ctx_h, &lens_ctx_h, sbl, ch_u, idx) as i32,
                                );
                                let lenv = len_c[ch_u][idx] as i32;
                                enc_s(enc, lnc, lenv);
                                let lbtv = lbt_c[ch_u][qp as usize];
                                let mut bi = lenv - 2;
                                while bi >= 0 {
                                    shift_b2(&mod_res, lenv, bi);
                                    enc_b(enc, &mod_res, ((lbtv >> bi) & 1) as i32);
                                    bi -= 1;
                                }
                            }
                        }
                        qp -= 1;
                    }
                    r -= 1;
                }

                // --- COEFFICIENTS FINISHED: UPDATE CONTEXT ---
                p = sv_bound;
                if p < 578 {
                    for k in 0..(578 - p) as usize {
                        abs_c[ch_u][p as usize + 1 + k] = 0;
                    }
                }
                if !j_coding || ch == 0 {
                    // channel 0 context
                    let cur_abs = if sbl == 0 {
                        &mut absl_ctx_h[ch_u]
                    } else {
                        &mut abss_ctx_h[ch_u]
                    };
                    let cur_sgn = if sbl == 0 {
                        &mut sgnl_ctx_h[ch_u]
                    } else {
                        &mut sgns_ctx_h[ch_u]
                    };
                    let cur_len = if sbl == 0 {
                        &mut lenl_ctx_h[ch_u]
                    } else {
                        &mut lens_ctx_h[ch_u]
                    };
                    for i in (0..578).rev() {
                        let idx = i + 1;
                        let ai = abs_c[ch_u][idx];
                        let am1 = abs_c[ch_u][idx - 1];
                        let ap1 = abs_c[ch_u][idx + 1];
                        cur_abs[idx] =
                            ((3 * ai as i32 + am1 as i32 + ap1 as i32 + 2 * cur_abs[idx] as i32 + 4)
                                / 7) as u8;
                        if ai > 0 {
                            cur_sgn[idx] =
                                (((cur_sgn[idx] as i32) << 1) | (sgn_c[ch_u][idx] & 0x1) as i32)
                                    as u8
                                    & 0xF;
                            if ai == 15 {
                                cur_len[idx] =
                                    ((2 * len_c[ch_u][idx] as i32 + cur_len[idx] as i32 + 2) / 3)
                                        as u8;
                            } else {
                                cur_len[idx] >>= 1;
                            }
                        } else {
                            cur_sgn[idx] = ((cur_sgn[idx] as i32) << 1) as u8 & 0xF;
                            cur_len[idx] >>= 1;
                        }
                    }
                    if j_coding {
                        // channel 1 context
                        let cur_abs = if sbl == 0 {
                            &mut absl_ctx_h[1]
                        } else {
                            &mut abss_ctx_h[1]
                        };
                        let cur_sgn = if sbl == 0 {
                            &mut sgnl_ctx_h[1]
                        } else {
                            &mut sgns_ctx_h[1]
                        };
                        let cur_len = if sbl == 0 {
                            &mut lenl_ctx_h[1]
                        } else {
                            &mut lens_ctx_h[1]
                        };
                        if p < 578 {
                            for k in 0..(578 - p) as usize {
                                cur_abs[p as usize + 1 + k] = 0;
                                cur_sgn[p as usize + 1 + k] = 2;
                                cur_len[p as usize + 1 + k] = 0;
                            }
                        }
                        for i in (0..p).rev() {
                            let idx = (i + 1) as usize;
                            let ai = abs_c[ch_u][idx];
                            cur_abs[idx] = ai;
                            if ai > 0 {
                                cur_sgn[idx] = sgn_c[ch_u][idx];
                                if ai == 15 {
                                    cur_len[idx] = len_c[ch_u][idx];
                                } else {
                                    cur_len[idx] = 0;
                                }
                            } else {
                                cur_sgn[idx] = 2;
                                cur_len[idx] = 0;
                            }
                        }
                    }
                }

                // ---> RECONSTRUCTION INFORMATION: STUFFING BITS <---
                let bitc_now = huffman.get_count();
                let mut n = lmaxp - bitc_now;
                if n == 0 {
                    // the preferred case
                    shift_s(&mod_nst, ctx_nst as i32);
                    enc_s(enc, &mod_nst, 0);
                    ctx_nst = 0;
                } else {
                    // encode # of stuffing bits (should be zero!)
                    while n >= STUFFING_STEP {
                        shift_s(&mod_nst, ctx_nst as i32);
                        enc_s(enc, &mod_nst, STUFFING_STEP);
                        ctx_nst = STUFFING_STEP as u8;
                        n -= STUFFING_STEP;
                    }
                    shift_s(&mod_nst, ctx_nst as i32);
                    enc_s(enc, &mod_nst, n);
                    ctx_nst = n as u8;
                    // encode stuffing bits
                    let mut bc = bitc_now;
                    while bc < lmaxp {
                        shift_b(&mod_bst, ctx_bst as i32);
                        let c = huffman.read_bit();
                        enc_b(enc, &mod_bst, c as i32);
                        ctx_bst = ((ctx_bst << 1) | c) & 0xF;
                        bc += 1;
                    }
                }

                // ---> GRANULE FINISHED! <---
                bitp += lmaxp;

                // write back sv bound / region bounds
                granule.sv_bound = sv_bound as i16;
                granule.region_bound = rb;
            }
        }

        // ---> ENCODE BITRATE <---
        if i_bitrate == -1 {
            // !!! (bit-reservoir?)
            shift_s(&mod_btr, bitrate_pred[frames[fi].main_size as usize]);
            enc_s(enc, &mod_btr, frames[fi].bits as i32);
        }

        // ---> RECONSTRUCTION INFORMATION: AUX DATA AND PADDING <---
        if !is_lastframe && i_bit_res != 0 {
            let next_fi = frames[fi].next.unwrap();
            let mut naux = if (frames[fi].aux_size + (frames[next_fi].bit_reservoir as i32)) < 511 {
                frames[fi].aux_size // byte size of aux data
            } else {
                511 - frames[next_fi].bit_reservoir as i32 // space taken from bit reservoir
            };
            while naux >= AUX_DATA_STEP {
                shift_s(&mod_aux, ctx_aux as i32);
                enc_s(enc, &mod_aux, AUX_DATA_STEP);
                ctx_aux = AUX_DATA_STEP as u8;
                naux -= AUX_DATA_STEP;
            }
            shift_s(&mod_aux, ctx_aux as i32);
            enc_s(enc, &mod_aux, naux);
            ctx_aux = naux as u8;
        }
        // # of padding and aux data bits
        let mut n = ((frames[fi].main_size + frames[fi].aux_size) * 8) - bitp;
        // locally store the padding and aux data
        let mut pna = 0usize;
        let mut ni = n;
        while ni >= 8 {
            pad_and_aux[pna] = huffman.read_bits(8) as u8;
            pna += 1;
            ni -= 8;
        }
        pad_and_aux[pna] = huffman.read_bits(ni) as u8;
        // make and check prediction
        if pmp_predict_lame_anc(anc, n, Some(&pad_and_aux)) {
            // prediction matches
            enc_b(enc, &mod_pap, 1);
        } else {
            // prediction doesn't match
            enc_b(enc, &mod_pap, 0);
            huffman.rewind_bits(n);
            ctx_pad = 0xFF;
            while n > 0 {
                shift_b(&mod_pad, ctx_pad as i32);
                let c = huffman.read_bit();
                enc_b(enc, &mod_pad, c as i32);
                ctx_pad = (ctx_pad << 1) | c;
                n -= 1;
            }
        }

        // ---> FRAME FINISHED! <----
    }

    Ok(())
}

/// decode the main data (scalefactors & coefficients + stuffing bits +
/// bitrate + aux data size & padding/aux bits) from the arithmetic coder
/// and reconstruct the MP3 main data stream.
pub fn pmp_decode_main_data(st: &mut PackState, dec: &mut Aricoder) -> Result<(), String> {
    let nch = st.g_nchannels as usize;

    // chain indices
    let chainf = frame_chain(st);
    let lastframe = st.lastframe;

    let i_samplerate = st.ai.i_samplerate as i32;
    let i_bitrate = st.ai.i_bitrate as i32;
    let i_bit_res = st.ai.i_bit_res as i32;
    let i_channels = st.ai.i_channels as i32;
    let j_coding = i_channels == MP3_STEREO || i_channels == MP3_JOINT_STEREO;
    let bitrate_pred: &[i32; 2049] = &MP3_BITRATE_PRED[i_samplerate as usize];

    // statistical models
    let mod_svb = wrap_s(mod_s(576 / 4 + 1 + 1, 576 / 4 + 1 + 1, 1));
    let mod_bvf = wrap_s(mod_s(576 / 2 + 1, 0, 0));
    let mod_nst = wrap_s(mod_s(STUFFING_STEP + 1, STUFFING_STEP + 1, 1));
    let mod_bst = wrap_b(mod_b(16, 1));
    let mod_pad = wrap_b(mod_b(256, 1));
    let mod_pap = wrap_b(mod_b(0, 0));
    let mod_aux = wrap_s(mod_s(AUX_DATA_STEP + 1, AUX_DATA_STEP + 1, 1));
    let mod_btr = wrap_s(mod_s(16, 16, 1));
    let mod_res = wrap_b(mod_b(13 + 1, 2));

    let mut mod_scf: [[[Option<Rc<RefCell<ModelS>>>; 10]; 4]; 2] = Default::default();
    let mut mod_abv: [[[Option<Rc<RefCell<ModelS>>>; 32]; 8]; 2] = Default::default();
    let mut mod_asv: [[[Option<Rc<RefCell<ModelB>>>; 2]; 8]; 2] = Default::default();
    let mut mod_sgn: [[Option<Rc<RefCell<ModelB>>>; 8]; 2] = Default::default();
    let mut mod_len: [[Option<Rc<RefCell<ModelS>>>; 14]; 2] = Default::default();

    const ABV_MAXS: [i32; 16] = [0, 1, 2, 2, 0, 3, 3, 5, 5, 5, 7, 7, 7, 15, 0, 15];
    for ch in 0..2usize {
        let ms = [2i32, 4, 8, 16];
        for (sl, scf_slots) in mod_scf[ch].iter_mut().enumerate() {
            for slot in scf_slots.iter_mut() {
                *slot = Some(wrap_s(mod_s(ms[sl], 16, 2)));
            }
        }
        let nflags = if j_coding && ch > 0 { 8 } else { 2 };
        for f in 0..nflags {
            mod_abv[ch][f][0] = None;
            for i in 1..16usize {
                if i == 4 || i == 14 {
                    mod_abv[ch][f][i] = None;
                } else {
                    mod_abv[ch][f][i] = Some(wrap_s(mod_s(ABV_MAXS[i] + 1, 16, 2)));
                }
            }
            let m16 = wrap_s(mod_s(15 + 1, 16, 2));
            for slot in mod_abv[ch][f][16..=23].iter_mut() {
                *slot = Some(Rc::clone(&m16));
            }
            let m24 = wrap_s(mod_s(15 + 1, 16, 2));
            for slot in mod_abv[ch][f][24..=31].iter_mut() {
                *slot = Some(Rc::clone(&m24));
            }
            mod_asv[ch][f][0] = Some(wrap_b(mod_b(16, 2)));
            mod_asv[ch][f][1] = Some(wrap_b(mod_b(16, 2)));
            mod_sgn[ch][f] = Some(wrap_b(mod_b(16, 2)));
        }
        for (i, slot) in mod_len[ch].iter_mut().enumerate().skip(1) {
            *slot = Some(wrap_s(mod_s(i as i32 + 1, 13 + 1, 1)));
        }
    }

    // local coefficient / scalefactor / context storage
    let mut scf_c: [Vec<u8>; 2] = [vec![0; 21], vec![0; 21]];
    let mut scf_l_long: [Vec<u8>; 2] = [vec![0; 21], vec![0; 21]];
    let mut scf_l_short: [Vec<u8>; 2] = [vec![0; 21], vec![0; 21]];
    let mut abs_c: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut sgn_c: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut len_c: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut lbt_c: [Vec<u32>; 2] = [vec![0; 576], vec![0; 576]];
    let mut absl_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut abss_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut sgnl_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut sgns_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut lenl_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];
    let mut lens_ctx_h: [Vec<u8>; 2] = [vec![0; 580], vec![0; 580]];

    // huffman writer for the reconstructed MP3 main data
    let mut huffman = HuffmanWriter::new(0);

    // reset ancillary predictor
    pmp_predict_lame_anc(&mut st.anc, -1, None);

    let frames = &mut st.frames;
    let granules = &mut st.granules;
    let anc = &mut st.anc;

    let mut bitp: i32;
    let mut flags = 0i32;
    let mut shared = [[0i8; 4]; 2];
    let mut ctx_svb = [0u8; 2];
    let mut ctx_nst = 0u8;
    let mut ctx_bst = 0u8;
    let mut ctx_aux = 0u8;
    let mut ctx_pad: u8;
    let mut bitres = 0i32;

    for &fi in &chainf {
        // record main index
        frames[fi].main_index = huffman.getpos() as i32;
        let stereo_ms = frames[fi].stereo_ms as i32;
        let is_lastframe = Some(fi) == lastframe;
        bitp = 0;
        for gr in 0..2i32 {
            for ch in 0..nch as i32 {
                let ch_u = ch as usize;
                let gi = frames[fi].granules[ch_u][gr as usize];
                let granule = &mut granules[gi];

                // --- MAIN DATA DECODING: PREPARATIONS ---
                let sbl = if granule.block_type as i32 == SHORT_BLOCK { 1 } else { 0 };
                let slen = SLENGTH_TABLE[granule.slength as usize];
                let mut rb = granule.region_bound;
                let rt = granule.region_table;
                let select_htabb = granule.select_htabb as i32;
                // set compression flags
                flags = if !j_coding || ch == 0 {
                    sbl
                } else {
                    sbl | (stereo_ms << 1) | ((flags ^ sbl) << 2)
                };
                // store sharing info
                if gr == 0 {
                    shared[ch_u][0] = (granule.share >> 3) & 0x1;
                    shared[ch_u][1] = (granule.share >> 2) & 0x1;
                    shared[ch_u][2] = (granule.share >> 1) & 0x1;
                    shared[ch_u][3] = granule.share & 0x1;
                }
                // reset counter
                huffman.reset_counter();
                // store bit reservoir state
                frames[fi].bit_reservoir = bitres as i16;

                // ---> SCALEFACTOR PROCESSING <---
                if sbl == 0 {
                    // long blocks
                    let mut ctx_scf = 0u8;
                    let mut p = 0i32;
                    for g in 0..4i32 {
                        let sl = slen[if g < 2 { 0 } else { 1 }];
                        if (gr != 0) && (shared[ch_u][g as usize] != 0) {
                            // shared
                            for k in 0..SCF_WIDTH[g as usize] as usize {
                                scf_c[ch_u][p as usize + k] = scf_l_long[ch_u][p as usize + k];
                            }
                            p = SCF_BOUNDS[g as usize];
                            ctx_scf = scf_c[ch_u][(p - 1) as usize];
                        } else if sl == 0 {
                            // zero slength
                            for k in 0..SCF_WIDTH[g as usize] as usize {
                                scf_c[ch_u][p as usize + k] = 0;
                            }
                            p = SCF_BOUNDS[g as usize];
                            ctx_scf = 0;
                        } else {
                            // non shared
                            let sfc = mod_scf[ch_u][(sl - 1) as usize]
                                [if shared[ch_u][g as usize] != 0 { (g | 4) as usize } else { g as usize }]
                                .as_ref()
                                .unwrap();
                            while p < SCF_BOUNDS[g as usize] {
                                shift_s2(sfc, ctx_scf as i32, scf_l_long[ch_u][p as usize] as i32);
                                let v = dec_s(dec, sfc);
                                scf_c[ch_u][p as usize] = v as u8;
                                huffman.write_bits(v as u32, sl);
                                ctx_scf = v as u8;
                                p += 1;
                            }
                        }
                    }
                    std::mem::swap(&mut scf_c[ch_u], &mut scf_l_long[ch_u]);
                } else {
                    // short blocks
                    for _i in 0..3i32 {
                        let mut ctx_scf = 0u8;
                        let mut p = 0i32;
                        for g in 0..2i32 {
                            let sl = slen[g as usize];
                            if sl == 0 {
                                for k in 0..SCF_LH_WIDTH_SHORT[g as usize] as usize {
                                    scf_c[ch_u][p as usize + k] = 0;
                                }
                                p = SCF_LH_BOUNDS_SHORT[g as usize];
                            } else {
                                let sfc = mod_scf[ch_u][(sl - 1) as usize][(g as usize) | 0x8]
                                    .as_ref()
                                    .unwrap();
                                while p < SCF_LH_BOUNDS_SHORT[g as usize] {
                                    shift_s2(sfc, ctx_scf as i32, scf_l_short[ch_u][p as usize] as i32);
                                    let v = dec_s(dec, sfc);
                                    scf_c[ch_u][p as usize] = v as u8;
                                    huffman.write_bits(v as u32, sl);
                                    ctx_scf = v as u8;
                                    p += 1;
                                }
                            }
                        }
                        std::mem::swap(&mut scf_c[ch_u], &mut scf_l_short[ch_u]);
                    }
                }

                // ---> COEFFICIENTS PROCESSING <---
                // --- DECODING AND FIXING OF SV BOUND ---
                if sbl == 0 {
                    shift_s(&mod_svb, ctx_svb[ch_u] as i32);
                } else {
                    shift_s(&mod_svb, 144 + 1);
                }
                let mut sv_bound: i32;
                let c = dec_s(dec, &mod_svb);
                if c <= 144 {
                    // for proper files
                    sv_bound = c << 2;
                    if rb[2] % 4 == 2 {
                        sv_bound += 2;
                    }
                    if sbl == 0 {
                        ctx_svb[ch_u] = c as u8;
                    }
                } else {
                    // for broken files
                    let c2 = dec_s(dec, &mod_bvf);
                    sv_bound = rb[2] as i32 - (c2 << 1);
                    // fix bounds
                    rb[2] = sv_bound as i16;
                    for i in (0..2).rev() {
                        if sv_bound < rb[i] as i32 {
                            rb[i] = sv_bound as i16;
                        } else {
                            break;
                        }
                    }
                }

                // --- COEFFICIENT DECODING: PREPARATIONS ---
                let mut ctx_abs = 0i32;
                let mut ctx_pat = 0u8;
                let rlb = rb[2] as i32;
                let mod_sgc = mod_sgn[ch_u][flags as usize].as_ref().unwrap();

                // --- COEFFICIENT DECODING: SMALL VALUES ---
                let mod_asc = mod_asv[ch_u][flags as usize][select_htabb as usize]
                    .as_ref()
                    .unwrap();
                let mut p = sv_bound - 1;
                while p >= rlb {
                    let idx = (p + 1) as usize;
                    shift_b2(
                        mod_asc,
                        ctx_pat as i32,
                        ctx_h_val(&absl_ctx_h, &abss_ctx_h, sbl, ch_u, idx) as i32,
                    );
                    let v = dec_b(dec, mod_asc);
                    abs_c[ch_u][idx] = v as u8;
                    ctx_pat = ((ctx_pat << 1) | v as u8) & 0xF;
                    ctx_abs = (2 * v + ctx_abs + 2) / 3;
                    if v == 1 {
                        shift_b2(
                            mod_sgc,
                            ctx_h_val(&absl_ctx_h, &abss_ctx_h, sbl, ch_u, idx) as i32,
                            ctx_h_val(&sgnl_ctx_h, &sgns_ctx_h, sbl, ch_u, idx) as i32,
                        );
                        let sv = dec_b(dec, mod_sgc);
                        sgn_c[ch_u][idx] = sv as u8;
                    }
                    p -= 1;
                }

                // --- COEFFICIENT DECODING: BIG VALUES ---
                let mut r = 2i32;
                while r >= 0 {
                    let rlb = if r == 0 { 0 } else { rb[(r - 1) as usize] as i32 };
                    if p < rlb {
                        r -= 1;
                        continue;
                    }
                    if rt[r as usize] == 0 {
                        for k in 0..(p - rlb) as usize {
                            abs_c[ch_u][rlb as usize + 1 + k] = 0;
                        }
                        p = rlb;
                        ctx_abs = 0;
                        r -= 1;
                        continue;
                    }
                    // set table and linbits
                    let bv_table = &BV_ENC_TABLE[rt[r as usize] as usize];
                    let linbits = bv_table.linbits as i32;
                    let mod_abc =
                        match mod_abv[ch_u][flags as usize][rt[r as usize] as usize].as_ref() {
                            Some(m) => m,
                            None => {
                                return Err(format!(
                                    "bad huffman table ({}) used (in frame #{})",
                                    rt[r as usize], frames[fi].n
                                ))
                            }
                        };
                    let mod_lnc = mod_len[ch_u][linbits as usize].as_ref();
                    while p >= rlb {
                        let idx = (p + 1) as usize;
                        shift_s2(
                            mod_abc,
                            ctx_abs,
                            ctx_h_val(&absl_ctx_h, &abss_ctx_h, sbl, ch_u, idx) as i32,
                        );
                        let v = dec_s(dec, mod_abc);
                        abs_c[ch_u][idx] = v as u8;
                        ctx_abs = (2 * v + ctx_abs + 2) / 3;
                        if v > 0 {
                            shift_b2(
                                mod_sgc,
                                ctx_h_val(&absl_ctx_h, &abss_ctx_h, sbl, ch_u, idx) as i32,
                                ctx_h_val(&sgnl_ctx_h, &sgns_ctx_h, sbl, ch_u, idx) as i32,
                            );
                            let sv = dec_b(dec, mod_sgc);
                            sgn_c[ch_u][idx] = sv as u8;
                            if linbits > 0 && v == 15 {
                                // loop invariant condition
                                let lnc = mod_lnc.unwrap();
                                shift_s(
                                    lnc,
                                    ctx_h_val(&lenl_ctx_h, &lens_ctx_h, sbl, ch_u, idx) as i32,
                                );
                                let lenv = dec_s(dec, lnc);
                                len_c[ch_u][idx] = lenv as u8;
                                let mut lbtv: u32;
                                if lenv > 0 {
                                    lbtv = 1;
                                    let mut bi = lenv - 2;
                                    while bi >= 0 {
                                        shift_b2(&mod_res, lenv, bi);
                                        let rv = dec_b(dec, &mod_res);
                                        lbtv = (lbtv << 1) | rv as u32;
                                        bi -= 1;
                                    }
                                } else {
                                    lbtv = 0;
                                }
                                lbt_c[ch_u][p as usize] = lbtv;
                            }
                        }
                        p -= 1;
                    }
                    r -= 1;
                }

                // --- COEFFICIENT ENCODING: BIG VALUES ---
                p = 0;
                for r in 0..3i32 {
                    if rt[r as usize] == 0 {
                        p = rb[r as usize] as i32;
                        continue;
                    }
                    let bv_table = &BV_ENC_TABLE[rt[r as usize] as usize];
                    let hcodes = match bv_table.h {
                        Some(h) => h,
                        None => {
                            return Err(format!(
                                "bad huffman table ({}) used (in frame #{})",
                                rt[r as usize], frames[fi].n
                            ))
                        }
                    };
                    let linbits = bv_table.linbits as i32;
                    while p < rb[r as usize] as i32 {
                        let pair = [
                            abs_c[ch_u][(p + 1) as usize],
                            abs_c[ch_u][(p + 2) as usize],
                        ];
                        huffman.encode_pair(hcodes, &pair);
                        for _ in 0..2 {
                            if abs_c[ch_u][(p + 1) as usize] > 0 {
                                if linbits > 0 && abs_c[ch_u][(p + 1) as usize] == 15 {
                                    huffman.write_bits(lbt_c[ch_u][p as usize], linbits);
                                }
                                huffman.write_bit(sgn_c[ch_u][(p + 1) as usize]);
                            }
                            p += 1;
                        }
                    }
                }

                // --- COEFFICIENT ENCODING: SMALL VALUES ---
                let hcode = if select_htabb != 0 { HTABB_ENC } else { HTABA_ENC };
                while p < sv_bound {
                    let quad = [
                        abs_c[ch_u][(p + 1) as usize],
                        abs_c[ch_u][(p + 2) as usize],
                        abs_c[ch_u][(p + 3) as usize],
                        abs_c[ch_u][(p + 4) as usize],
                    ];
                    huffman.encode_quadruple(hcode, &quad);
                    for _ in 0..4 {
                        if abs_c[ch_u][(p + 1) as usize] != 0 {
                            huffman.write_bit(sgn_c[ch_u][(p + 1) as usize]);
                        }
                        p += 1;
                    }
                }

                // --- COEFFICIENTS FINISHED: UPDATE CONTEXT ---
                if p < 578 {
                    for k in 0..(578 - p) as usize {
                        abs_c[ch_u][p as usize + 1 + k] = 0;
                    }
                }
                if !j_coding || ch == 0 {
                    // channel 0 context
                    let cur_abs = if sbl == 0 {
                        &mut absl_ctx_h[ch_u]
                    } else {
                        &mut abss_ctx_h[ch_u]
                    };
                    let cur_sgn = if sbl == 0 {
                        &mut sgnl_ctx_h[ch_u]
                    } else {
                        &mut sgns_ctx_h[ch_u]
                    };
                    let cur_len = if sbl == 0 {
                        &mut lenl_ctx_h[ch_u]
                    } else {
                        &mut lens_ctx_h[ch_u]
                    };
                    for i in (0..578).rev() {
                        let idx = i + 1;
                        let ai = abs_c[ch_u][idx];
                        let am1 = abs_c[ch_u][idx - 1];
                        let ap1 = abs_c[ch_u][idx + 1];
                        cur_abs[idx] =
                            ((3 * ai as i32 + am1 as i32 + ap1 as i32 + 2 * cur_abs[idx] as i32 + 4)
                                / 7) as u8;
                        if ai > 0 {
                            cur_sgn[idx] =
                                (((cur_sgn[idx] as i32) << 1) | (sgn_c[ch_u][idx] & 0x1) as i32)
                                    as u8
                                    & 0xF;
                            if ai == 15 {
                                cur_len[idx] =
                                    ((2 * len_c[ch_u][idx] as i32 + cur_len[idx] as i32 + 2) / 3)
                                        as u8;
                            } else {
                                cur_len[idx] >>= 1;
                            }
                        } else {
                            cur_sgn[idx] = ((cur_sgn[idx] as i32) << 1) as u8 & 0xF;
                            cur_len[idx] >>= 1;
                        }
                    }
                    if j_coding {
                        // channel 1 context
                        let cur_abs = if sbl == 0 {
                            &mut absl_ctx_h[1]
                        } else {
                            &mut abss_ctx_h[1]
                        };
                        let cur_sgn = if sbl == 0 {
                            &mut sgnl_ctx_h[1]
                        } else {
                            &mut sgns_ctx_h[1]
                        };
                        let cur_len = if sbl == 0 {
                            &mut lenl_ctx_h[1]
                        } else {
                            &mut lens_ctx_h[1]
                        };
                        if p < 578 {
                            for k in 0..(578 - p) as usize {
                                cur_abs[p as usize + 1 + k] = 0;
                                cur_sgn[p as usize + 1 + k] = 2;
                                cur_len[p as usize + 1 + k] = 0;
                            }
                        }
                        for i in (0..p).rev() {
                            let idx = (i + 1) as usize;
                            let ai = abs_c[ch_u][idx];
                            cur_abs[idx] = ai;
                            if ai > 0 {
                                cur_sgn[idx] = sgn_c[ch_u][idx];
                                if ai == 15 {
                                    cur_len[idx] = len_c[ch_u][idx];
                                } else {
                                    cur_len[idx] = 0;
                                }
                            } else {
                                cur_sgn[idx] = 2;
                                cur_len[idx] = 0;
                            }
                        }
                    }
                }

                // ---> RECONSTRUCTION INFORMATION: STUFFING BITS <---
                let mut n = 0i32;
                let mut c = STUFFING_STEP;
                while c == STUFFING_STEP {
                    // find out # of stuffing bits
                    shift_s(&mod_nst, ctx_nst as i32);
                    c = dec_s(dec, &mod_nst);
                    ctx_nst = c as u8;
                    n += c;
                }
                while n > 0 {
                    shift_b(&mod_bst, ctx_bst as i32);
                    c = dec_b(dec, &mod_bst);
                    huffman.write_bit(c as u8);
                    ctx_bst = ((ctx_bst << 1) | c as u8) & 0xF;
                    n -= 1;
                }

                // ---> GRANULE FINISHED! <---
                granule.main_data_bit = huffman.get_count() as i16;
                bitp += granule.main_data_bit as i32;

                // write back sv bound / region bounds
                granule.sv_bound = sv_bound as i16;
                granule.region_bound = rb;
            }
        }

        // --- STORE FRAME MAIN DATA SIZES ---
        frames[fi].main_bits = bitp;
        frames[fi].main_size = (bitp + 7) / 8;

        // ---> DECODE BITRATE <---
        if i_bitrate == -1 {
            shift_s(&mod_btr, bitrate_pred[frames[fi].main_size as usize]);
            frames[fi].bits = dec_s(dec, &mod_btr) as i8;
        }
        // calculate size of frame - use the lookup table
        frames[fi].frame_size = mp3_frame_size_table(i_samplerate)[frames[fi].bits as usize];
        if frames[fi].padding != 0 {
            frames[fi].frame_size += 1;
        }

        // ---> RECONSTRUCTION INFORMATION: AUX DATA AND PADDING <---
        if i_bit_res != 0 {
            // preliminary next frame bit reservoir size
            bitres = frames[fi].frame_size
                - frames[fi].fixed_size
                + frames[fi].bit_reservoir as i32
                - frames[fi].main_size;
            if !is_lastframe {
                let mut naux = if bitres < 511 { 0 } else { bitres - 511 };
                let mut c = AUX_DATA_STEP;
                while c == AUX_DATA_STEP {
                    // byte size of aux data
                    shift_s(&mod_aux, ctx_aux as i32);
                    c = dec_s(dec, &mod_aux);
                    ctx_aux = c as u8;
                    naux += c;
                }
                frames[fi].aux_size = naux;
            } else {
                frames[fi].aux_size = bitres;
            }
            // finalize size of bit reservoir
            bitres -= frames[fi].aux_size;
        } else {
            bitres = 0;
            frames[fi].aux_size = frames[fi].frame_size
                - frames[fi].fixed_size
                + frames[fi].bit_reservoir as i32
                - frames[fi].main_size;
        }
        // # of padding and aux data bits
        let mut n = ((frames[fi].main_size + frames[fi].aux_size) * 8) - bitp;
        // check if data can be predicted 100%
        if dec_b(dec, &mod_pap) == 1 {
            // prediction matches!
            let _ = pmp_predict_lame_anc(anc, n, None);
            let mut pna = 0usize;
            while n >= 8 {
                huffman.write_bits(anc.pred[pna] as u32, 8);
                pna += 1;
                n -= 8;
            }
            if n > 0 {
                huffman.write_bits((anc.pred[pna] as u32) >> (8 - n), n);
            }
        } else {
            // prediction doesn't match (d'oh!)
            let mut pad_and_aux = vec![0u8; 2048];
            let mut pna = 0usize;
            ctx_pad = 0xFF;
            let mut c: i32;
            let mut i = 1i32;
            while i <= n {
                shift_b(&mod_pad, ctx_pad as i32);
                c = dec_b(dec, &mod_pad);
                huffman.write_bit(c as u8);
                ctx_pad = (ctx_pad << 1) | c as u8;
                pad_and_aux[pna] = (pad_and_aux[pna] << 1) | c as u8;
                if i % 8 == 0 {
                    pna += 1;
                    pad_and_aux[pna] = 0;
                }
                i += 1;
            }
            // C++: `if (pmp_predict_lame_anc(n, pad_and_aux) == NULL) { error }`
            // -> the error fires when the prediction matches the decoded data
            if pmp_predict_lame_anc(anc, n, Some(&pad_and_aux)) {
                return Err("ancilary prediction error, please report".to_string());
            }
        }

        // ---> FRAME FINISHED! <----
    }

    // --- GRAB POINTER AND SIZE ---
    st.main_data = huffman.getptr();
    st.main_data_size = huffman.getpos() as i32;

    // --- CALCULATE MP3 FILE SIZE ---
    let lastframe_idx = lastframe.unwrap();
    st.mp3filesize = st.data_before_size
        + st.data_after_size
        + st.main_data_size
        + st.g_nframes * frames[lastframe_idx].fixed_size;

    Ok(())
}
