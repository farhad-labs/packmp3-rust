// mp3.rs - MPEG-1 Layer III frame parsing/building and related helpers.
// Faithful port of the MP3-specific functions in source/packmp3.cpp.
// `>> 0`/`<< 0`/`& 0x1` appear on purpose: they mirror the C++ bit-extraction
// expressions line for line.
#![allow(clippy::identity_op)]

use crate::bitops::{AbitReader, AbitWriter};
use crate::huffmp3::HuffmanReader;
use crate::hufftables::{BV_DEC_TABLE, HTABA_DEC, HTABB_DEC, HuffmanConvSet};
use crate::state::{Frame, GranuleInfo, GranuleData, PackState, EL_FATAL, EL_WARNING};
use crate::tables::{
    LAYER_I, MP3_MONO, SHORT_BLOCK, LONG_BLOCK, CRC_TABLE, SLENGTH_TABLE, SCF_BOUNDS,
    SCF_WIDTH, SCF_BOUNDS_SHORT, GARBAGE_TOLERANCE,
    mp3_bandwidth_bounds, mp3_bandwidth_bounds_short, mp3_frame_size_table,
};

/// reads and parses one MP3 frame header + side info. returns frame arena index.
pub fn mp3_read_frame(st: &mut PackState, data: &[u8], max_size: i32) -> Option<usize> {
    // immediately return if not enough data (min size for header and side info = 21)
    if max_size < 21 {
        return None;
    }

    // --- frame header ---
    let header = &data[0..4];

    // check syncword
    if header[0] != 0xFF || (header[1] & 0xFA) != 0xFA {
        // no syncword or not MPEG-1 LAYER III
        return None;
    }

    // build frame structure in the arena
    let mut frame = Frame::new();
    frame.n = 0;

    // extract data
    frame.mpeg = ((header[1] >> 3) & 0x3) as i8;
    frame.layer = ((header[1] >> 1) & 0x3) as i8;
    frame.protection = (((header[1] >> 0) & 0x1) ^ 1) as i8;
    frame.bits = ((header[2] >> 4) & 0xF) as i8;
    frame.samples = ((header[2] >> 2) & 0x3) as i8;
    frame.padding = ((header[2] >> 1) & 0x1) as i8;
    frame.privbit = ((header[2] >> 0) & 0x1) as i8;
    frame.channels = ((header[3] >> 6) & 0x3) as i8;
    frame.stereo_ms = ((header[3] >> 5) & 0x1) as i8;
    frame.stereo_int = ((header[3] >> 4) & 0x1) as i8;
    frame.copyright = ((header[3] >> 3) & 0x1) as i8;
    frame.original = ((header[3] >> 2) & 0x1) as i8;
    frame.emphasis = ((header[3] >> 0) & 0x3) as i8;

    // check data for problems (also contains the never used free form flag)
    if frame.bits == 0x0 || frame.bits == 0xF || frame.samples == 0x3 {
        return None;
    }

    // number of channels
    frame.nchannels = if frame.channels == MP3_MONO as i8 { 1 } else { 2 };
    let nch = frame.nchannels;

    // alloc memory for granules
    frame.granules = Vec::with_capacity(nch as usize);
    for _ in 0..nch {
        let g0 = st.alloc_granule(GranuleInfo::default());
        let g1 = st.alloc_granule(GranuleInfo::default());
        frame.granules.push([g0, g1]);
    }

    // calculate size of frame - use the lookup table
    frame.frame_size = mp3_frame_size_table(frame.samples as i32)[frame.bits as usize];
    if frame.padding != 0 {
        frame.frame_size += 1;
    }

    // --- side information ---
    let nsb: i32 = if nch == 1 { 17 } else { 32 };
    frame.fixed_size = 4 + nsb + if frame.protection != 0 { 2 } else { 0 };
    // check if enough data is available
    if frame.fixed_size > max_size {
        return None;
    }

    let side_start: usize;
    if frame.protection == 0x1 {
        side_start = 6;
        let sideinfo = &data[side_start..side_start + nsb as usize];
        // if there is a crc: check and discard
        let crc = ((data[4] as u16) << 8) + (data[5] as u16);
        if crc != mp3_calc_layer3_crc(&data[0..4], sideinfo, nsb) {
            st.errormessage = "crc checksum mismatch".to_string();
            st.errorlevel = EL_WARNING; // (!!!) careful - file might be broken
        }
    } else {
        side_start = 4;
    }
    let sideinfo = &data[side_start..side_start + nsb as usize];
    let mut side_reader = AbitReader::new(sideinfo);

    // frame global side info
    frame.bit_reservoir = side_reader.read(9) as i16;
    frame.padbits = side_reader.read(if nch == 1 { 5 } else { 3 }) as i8;
    for ch in 0..nch as usize {
        frame.granule_mut(st, ch, 0).share = side_reader.read(4) as i8;
        frame.granule_mut(st, ch, 1).share = 0x0;
    }
    // granule specific side info
    for gr in 0..2 {
        for ch in 0..nch as usize {
            let (main_data_bit, big_val_pairs, global_gain, slength, window_switching) = (
                side_reader.read(12) as i16,
                side_reader.read(9) as i16,
                side_reader.read(8) as i16,
                side_reader.read(4) as i8,
                side_reader.read(1) as i8,
            );
            let granule = frame.granule_mut(st, ch, gr);
            granule.main_data_bit = main_data_bit;
            granule.big_val_pairs = big_val_pairs;
            granule.global_gain = global_gain;
            granule.slength = slength;
            granule.window_switching = window_switching;
            if window_switching == 0 {
                // for normal blocks
                granule.region_table[0] = side_reader.read(5) as i8;
                granule.region_table[1] = side_reader.read(5) as i8;
                granule.region_table[2] = side_reader.read(5) as i8;
                granule.region0_size = side_reader.read(4) as i8;
                granule.region1_size = side_reader.read(3) as i8;
                if granule.region0_size + granule.region1_size > 20 {
                    st.errormessage = "region size out of bounds".to_string();
                    st.errorlevel = EL_FATAL;
                    return None;
                }
                let bounds = mp3_bandwidth_bounds(frame.samples as i32);
                granule.region_bound[0] = bounds[(granule.region0_size + 1) as usize] as i16;
                granule.region_bound[1] = bounds
                    [(granule.region0_size + granule.region1_size + 2) as usize]
                    as i16;
                granule.region_bound[2] = granule.big_val_pairs << 1;
                if granule.region_bound[0] > granule.region_bound[2] {
                    granule.region_bound[0] = granule.region_bound[2];
                    granule.region_bound[1] = granule.region_bound[2];
                } else if granule.region_bound[1] > granule.region_bound[2] {
                    granule.region_bound[1] = granule.region_bound[2];
                }
                granule.block_type = LONG_BLOCK as i8;
                granule.mixed_flag = 0;
                granule.sb_gain = [0; 3];
            } else {
                // for special blocks
                granule.block_type = side_reader.read(2) as i8;
                granule.mixed_flag = side_reader.read(1) as i8;
                granule.region_table[0] = side_reader.read(5) as i8;
                granule.region_table[1] = side_reader.read(5) as i8;
                granule.sb_gain[0] = side_reader.read(3) as i8;
                granule.sb_gain[1] = side_reader.read(3) as i8;
                granule.sb_gain[2] = side_reader.read(3) as i8;
                if granule.block_type != SHORT_BLOCK as i8 {
                    // region sizes for different block types
                    granule.region0_size = 8;
                    granule.region1_size = 0;
                    let bounds = mp3_bandwidth_bounds(frame.samples as i32);
                    granule.region_bound[0] = bounds[8] as i16;
                } else {
                    // special treatment for mixed blocks needed (!)
                    granule.region0_size = 9;
                    granule.region1_size = 0;
                    let bounds = mp3_bandwidth_bounds_short(frame.samples as i32);
                    granule.region_bound[0] = (bounds[9 / 3] * 3) as i16;
                }
                granule.region_bound[1] = granule.big_val_pairs << 1;
                if granule.region_bound[0] > granule.region_bound[1] {
                    granule.region_bound[0] = granule.region_bound[1];
                }
                granule.region_bound[2] = granule.region_bound[1];
                granule.region_table[2] = 0;
            }
            granule.preemphasis = side_reader.read(1) as i8;
            granule.coarse_scalefactors = side_reader.read(1) as i8;
            granule.select_htabb = side_reader.read(1) as i8;
            granule.sv_bound = 0;
            // check for obvious problems/contradictions
             if granule.main_data_bit == 0 && (granule.big_val_pairs != 0 || granule.slength != 0) {
                frame.n = -1; // mistreat frame number as trouble indicator :-)
            }
        }
    }

    // calculate total size of main (not aux) data (in byte)
    frame.main_bits = 0;
    for gr in 0..2 {
        for ch in 0..nch as usize {
            frame.main_bits += frame.granule(st, ch, gr).main_data_bit as i32;
        }
    }
    frame.main_size = (frame.main_bits + 7) >> 3;

    // calculate temporary size of aux data (in byte), subject to change
    frame.aux_size =
        frame.frame_size - frame.fixed_size + frame.bit_reservoir as i32 - frame.main_size;

    Some(st.alloc_frame(frame))
}

/// build frame from i_variables
pub fn mp3_build_frame(st: &mut PackState) -> Option<usize> {
    let mut frame = Frame::new();

    let ai = st.ai.clone();
    frame.mpeg = ai.i_mpeg;
    frame.layer = ai.i_layer;
    frame.protection = ai.i_protection;
    frame.bits = ai.i_bitrate;
    frame.samples = ai.i_samplerate;
    frame.padding = ai.i_padding;
    frame.privbit = ai.i_privbit;
    frame.channels = ai.i_channels;
    frame.stereo_ms = ai.i_stereo_ms;
    frame.stereo_int = ai.i_stereo_int;
    frame.copyright = ai.i_copyright;
    frame.original = ai.i_original;
    frame.emphasis = ai.i_emphasis;
    frame.bit_reservoir = 0;
    frame.padbits = ai.i_padbits;

    // stuff that is known right now (# channels and fixed size)
    if frame.channels == MP3_MONO as i8 {
        frame.fixed_size = if frame.protection != 0 { 4 + 2 + 17 } else { 4 + 17 };
        frame.nchannels = 1;
    } else {
        frame.fixed_size = if frame.protection != 0 { 4 + 2 + 32 } else { 4 + 32 };
        frame.nchannels = 2;
    }
    let nch = frame.nchannels;

    // alloc memory for granules
    frame.granules = Vec::with_capacity(nch as usize);
    for _ in 0..nch {
        let g0 = st.alloc_granule(GranuleInfo::default());
        let g1 = st.alloc_granule(GranuleInfo::default());
        frame.granules.push([g0, g1]);
    }

    // granule specific stuff
    for ch in 0..nch as usize {
        for gr in 0..2 {
            let granule = frame.granule_mut(st, ch, gr);
            granule.share = if gr == 0 { ai.i_share } else { 0 };
            granule.window_switching = ai.i_sblocks;
            granule.mixed_flag = ai.i_mixed;
            granule.block_type = LONG_BLOCK as i8;
            granule.sb_gain[0] = ai.i_sbgain;
            granule.sb_gain[1] = ai.i_sbgain;
            granule.sb_gain[2] = ai.i_sbgain;
            granule.preemphasis = ai.i_preemphasis;
            granule.coarse_scalefactors = ai.i_coarse;
        }
    }

    Some(st.alloc_frame(frame))
}

/// append frame to the frame chain
pub fn mp3_append_frame(st: &mut PackState, frame: usize) -> bool {
    // running frame counter (static in C++); reset when the chain is empty
    let n = if let Some(last) = st.lastframe {
        st.frames[last].n + 1
    } else {
        0
    };

    // insert frame into the frame chain, set up links between frames
    // aux size correction has to take place elsewhere
    if st.firstframe.is_none() {
        st.firstframe = Some(frame);
        st.frames[frame].prev = None;
    } else {
        let last = st.lastframe.unwrap();
        st.frames[last].next = Some(frame);
        st.frames[frame].prev = Some(last);
    }
    st.lastframe = Some(frame);

    // set up links for granules between each other
    let nch = st.frames[frame].nchannels;
    for ch in 0..nch as usize {
        let [g0, g1] = st.frames[frame].granules[ch];
        st.granules[g0].n = n << 1;
        st.granules[g1].n = (st.granules[g0].n) | 1;
        st.granules[g0].next = Some(g1);
        st.granules[g1].prev = Some(g0);
        st.granules[g1].next = None;
        // link to previous frame's granules per channel (the C++ code keeps a
        // static lastgranule[2] across calls; the chain stores it implicitly)
        let prev_g1 = st.frames[frame].prev.map(|p| st.frames[p].granules[ch][1]);
        st.granules[g0].prev = prev_g1;
        if let Some(pg1) = prev_g1 {
            st.granules[pg1].next = Some(g0);
        }
    }

    // some generic stuff
    st.frames[frame].n = n;
    st.frames[frame].next = None;

    true
}

/// discard frame data
///
/// In the C++ original this frees the frame's memory. It has no observable
/// effect on output (discarded frames are never in the frame chain), so in the
/// arena model this is a no-op; the arena is cleared wholesale by reset_buffers.
pub fn mp3_discard_frame(_st: &mut PackState, _frame: usize) -> bool {
    true
}

/// mute a single frame (dangerous!)
pub fn mp3_mute_frame(st: &mut PackState, frame: usize) -> bool {
    let nch = st.frames[frame].nchannels;
    let ums: i32 = 2 + (nch as i32 * 2 * 4);

    // store reconstruction data
    if st.unmute_data_size == 0 {
        st.unmute_data = vec![0u8; 1];
        st.unmute_data_size = 1;
    }
    st.unmute_data.resize((st.unmute_data_size + ums) as usize, 0);
    let mut ptr = st.unmute_data_size as usize;
    st.unmute_data_size += ums;

    // snapshot the frame fields we need
    let (privbit, original, copyright, bit_reservoir) = {
        let f = &st.frames[frame];
        (f.privbit, f.original, f.copyright, f.bit_reservoir)
    };
    let mut byte = (privbit as u8 & 0x1) << 7;
    byte |= (original as u8 & 0x1) << 6;
    byte |= (copyright as u8 & 0x1) << 5;
    byte |= ((bit_reservoir >> 8) & 0x1) as u8;
    st.unmute_data[ptr] = byte;
    ptr += 1;
    st.unmute_data[ptr] = (bit_reservoir & 0xFF) as u8;
    ptr += 1;
    for ch in 0..nch as usize {
        for gr in 0..2 {
            let gidx = st.frames[frame].granules[ch][gr];
            let g = &st.granules[gidx];
            st.unmute_data[ptr] = (g.main_data_bit >> 4) as u8;
            ptr += 1;
            st.unmute_data[ptr] =
                ((((g.main_data_bit as i32) << 4) as u8) & 0xF0) | (g.slength as u8 & 0x0F);
            ptr += 1;
            st.unmute_data[ptr] = (g.big_val_pairs >> 1) as u8;
            ptr += 1;
            st.unmute_data[ptr] = (((g.big_val_pairs as i32) << 7) as u8) & 0x80;
            ptr += 1;
        }
    }
    // take count - careful this doesn't excede 255
    st.unmute_data[0] = st.unmute_data[0].wrapping_add(1);

    // mute frame - this has to be done in order
    // we assume that all previous frames are muted, too!
    // move aux data around the bit reservoirs only for files that already provide a
    // bit reservoir. the decoder depends on this.
    let (prev, next, bit_res_used) = {
        let f = &st.frames[frame];
        (f.prev, f.next, st.ai.i_bit_res != 0)
    };
    if let Some(p) = prev {
        if bit_res_used {
            // make some room, use prev frame aux too!
            let prev_aux = st.frames[p].aux_size;
            // C++: short += int (truncating assignment back to short)
            let mut br = (st.frames[frame].bit_reservoir as i32 + prev_aux) as i16;
            if br >= 512 {
                st.frames[p].aux_size = br as i32 - 511;
                br = 511;
            } else {
                st.frames[p].aux_size = 0;
            }
            st.frames[frame].bit_reservoir = br;
        } else {
            st.frames[frame].bit_reservoir = 0;
        }
    } else {
        st.frames[frame].bit_reservoir = 0;
    }
    {
        let next_res = next.map(|n| st.frames[n].bit_reservoir);
        let f = &mut st.frames[frame];
        f.aux_size = f.frame_size - f.fixed_size + f.bit_reservoir as i32;
        if let Some(nr) = next_res {
            f.aux_size -= nr as i32;
        }
        f.main_size = 0;
        f.main_bits = 0;
    }
    for ch in 0..nch as usize {
        for gr in 0..2 {
            let gidx = st.frames[frame].granules[ch][gr];
            let granule = &mut st.granules[gidx];
            granule.main_data_bit = 0;
            granule.big_val_pairs = 0;
            granule.slength = 0;
            // fix region bounds
            granule.region_bound[0] = 0;
            granule.region_bound[1] = 0;
            granule.region_bound[2] = 0;
        }
    }

    true
}

/// unmute a single frame (restore to broken state)
pub fn mp3_unmute_frame(st: &mut PackState, frame: usize) -> bool {
    // restore a single frame to its original, broken state
    // enough fix data has to be present, channel mode has to be consistent
    let nch = st.g_nchannels;
    let ums = 2 + (nch * 2 * 4);
    let mut ptr = (1 + (st.n_bad_first * ums)) as usize;

    let f = &mut st.frames[frame];
    f.privbit = ((st.unmute_data[ptr] >> 7) & 0x1) as i8;
    f.original = ((st.unmute_data[ptr] >> 6) & 0x1) as i8;
    f.copyright = ((st.unmute_data[ptr] >> 5) & 0x1) as i8;
    f.bit_reservoir = ((st.unmute_data[ptr] as i32) << 8) as i16;
    ptr += 1;
    f.bit_reservoir |= st.unmute_data[ptr] as i16;
    ptr += 1;
    for ch in 0..nch as usize {
        for gr in 0..2 {
            let gidx = st.frames[frame].granules[ch][gr];
            let granule = &mut st.granules[gidx];
            granule.main_data_bit = ((st.unmute_data[ptr] as i32) << 4) as i16;
            ptr += 1;
            granule.main_data_bit |= (st.unmute_data[ptr] >> 4) as i16;
            granule.slength = (st.unmute_data[ptr] & 0x0F) as i8;
            ptr += 1;
            granule.big_val_pairs = ((st.unmute_data[ptr] as i32) << 1) as i16;
            ptr += 1;
            granule.big_val_pairs |= (st.unmute_data[ptr] >> 7) as i16;
            ptr += 1;
        }
    }
    // no need to accomodate for the changed frame params - for now (!!!)

    true
}

/// build fixed part of physical frame, returns the fixed bytes
pub fn mp3_build_fixed(st: &mut PackState, frame: usize) -> Vec<u8> {
    let f = &st.frames[frame];
    let nch = f.nchannels;
    let nsb: i32 = if nch == 1 { 17 } else { 32 };
    let fixed_size = f.fixed_size as usize;
    let side_start = if f.protection != 0 { 4 + 2 } else { 4 };

    let mut header = [0u8; 4];
    let mut sideinfo = vec![0u8; nsb as usize];

    // --- frame header ---
    header[0] = 0xFF;
    header[1] = 0xFA;
    header[1] |= (f.mpeg as u8) << 3;
    header[1] |= (f.layer as u8) << 1;
    header[1] |= (f.protection as u8) ^ 1;
    header[2] |= (f.bits as u8) << 4;
    header[2] |= (f.samples as u8) << 2;
    header[2] |= (f.padding as u8) << 1;
    header[2] |= (f.privbit as u8) << 0;
    header[3] |= (f.channels as u8) << 6;
    header[3] |= (f.stereo_ms as u8) << 5;
    header[3] |= (f.stereo_int as u8) << 4;
    header[3] |= (f.copyright as u8) << 3;
    header[3] |= (f.original as u8) << 2;
    header[3] |= (f.emphasis as u8) << 0;

    // --- side information ---
    let mut side_writer = AbitWriter::new(64);
    side_writer.write(f.bit_reservoir as u32, 9);
    side_writer.write(f.padbits as u32, if nch == 1 { 5 } else { 3 });
    for ch in 0..nch as usize {
        side_writer.write(f.granule(st, ch, 0).share as u32, 4);
    }
    for gr in 0..2 {
        for ch in 0..nch as usize {
            let granule = f.granule(st, ch, gr);
            side_writer.write(granule.main_data_bit as u32, 12);
            side_writer.write(granule.big_val_pairs as u32, 9);
            side_writer.write(granule.global_gain as u32, 8);
            side_writer.write(granule.slength as u32, 4);
            side_writer.write(granule.window_switching as u32, 1);
            if granule.window_switching == 0 {
                // for normal blocks
                side_writer.write(granule.region_table[0] as u32, 5);
                side_writer.write(granule.region_table[1] as u32, 5);
                side_writer.write(granule.region_table[2] as u32, 5);
                side_writer.write(granule.region0_size as u32, 4);
                side_writer.write(granule.region1_size as u32, 3);
            } else {
                // for special blocks
                side_writer.write(granule.block_type as u32, 2);
                side_writer.write(granule.mixed_flag as u32, 1);
                side_writer.write(granule.region_table[0] as u32, 5);
                side_writer.write(granule.region_table[1] as u32, 5);
                side_writer.write(granule.sb_gain[0] as u32, 3);
                side_writer.write(granule.sb_gain[1] as u32, 3);
                side_writer.write(granule.sb_gain[2] as u32, 3);
            }
            side_writer.write(granule.preemphasis as u32, 1);
            side_writer.write(granule.coarse_scalefactors as u32, 1);
            side_writer.write(granule.select_htabb as u32, 1);
        }
    }
    let tmp = side_writer.getptr();
    sideinfo.copy_from_slice(&tmp[..nsb as usize]);

    // assemble fixed bytes
    let mut fixed = vec![0u8; fixed_size];
    fixed[0..4].copy_from_slice(&header);
    fixed[side_start..side_start + nsb as usize].copy_from_slice(&sideinfo);

    // --- ...and optional crc checksum ---
    if f.protection == 0x1 {
        let crc = mp3_calc_layer3_crc(&header, &sideinfo, nsb);
        fixed[4] = ((crc >> 8) & 0xFF) as u8;
        fixed[5] = ((crc >> 0) & 0xFF) as u8;
    }

    fixed
}

/// seeks for the first proper MPEG audio frame
pub fn mp3_seek_firstframe(data: &[u8], size: i32) -> i32 {
    let mut mpeg: i32 = -1;
    let mut layer: i32 = -1;
    let mut samples: i32 = -1;
    let mut channels: i32 = -1;
    let mut protection: i32 = -1;

    let mut pos0 = mp3_get_id3_size(data, size);

    // calculate last tolerable seek position
    let mut pos1 = pos0 + GARBAGE_TOLERANCE;
    if pos1 > size - 4 {
        pos1 = size - 4;
    }

    // mp3 frame seeker loop
    // conditions for proper first frame: 5 consecutive frames,
    // same channel, mpeg, layer, samples setting
    let result;
    loop {
        // seek for first frame candidate
        while pos0 < pos1 {
            if data[pos0 as usize] == 0xFF && (data[pos0 as usize + 1] & 0xE0) == 0xE0 {
                break;
            }
            pos0 += 1;
        }
        // nothing found -> give up
        if pos0 == pos1 {
            result = -1;
            break;
        }
        // check for consecutive frames
        let mut n = 0;
        let mut pos = pos0;
        let mut frame_size;
        while n < 5 && pos < size - 4 {
            // check syncword
            if data[pos as usize] != 0xFF || (data[pos as usize + 1] & 0xE0) != 0xE0 {
                break;
            }
            // extract and store or compare data from header
            if n == 0 {
                mpeg = ((data[pos as usize + 1] >> 3) & 0x3) as i32;
                layer = ((data[pos as usize + 1] >> 1) & 0x3) as i32;
                protection = ((data[pos as usize + 1] >> 0) & 0x1) as i32;
                samples = ((data[pos as usize + 2] >> 2) & 0x3) as i32;
                channels = ((data[pos as usize + 3] >> 6) & 0x3) as i32;
            } else if (mpeg != ((data[pos as usize + 1] >> 3) & 0x3) as i32)
                || (layer != ((data[pos as usize + 1] >> 1) & 0x3) as i32)
                || (protection != ((data[pos as usize + 1] >> 0) & 0x1) as i32)
                || (samples != ((data[pos as usize + 2] >> 2) & 0x3) as i32)
                || (channels != ((data[pos as usize + 3] >> 6) & 0x3) as i32)
            {
                break;
            }
            let bits = ((data[pos as usize + 2] >> 4) & 0xF) as i32;
            let padding = ((data[pos as usize + 2] >> 1) & 0x1) as i32;
            // check for problems
            if mpeg == 0x1 || layer == 0x0 || bits == 0x0 || bits == 0xF || samples == 0x3 {
                break;
            }
            // find out frame size
            frame_size = crate::tables::FRAME_SIZE_TABLE
                [mpeg as usize][layer as usize][samples as usize][bits as usize];
            if padding != 0 {
                frame_size += if layer == LAYER_I { 4 } else { 1 };
            }
            n += 1;
            pos += frame_size;
        }
        // consider first frame proper if 5 consecutive frames are found
        if n >= 5 {
            result = pos0;
            break;
        }
        pos0 += 1;
    }

    result
}

/// extract ID3v2 tag from beginning of file
pub fn mp3_get_id3_size(id3tag: &[u8], max_size: i32) -> i32 {
    const ID3V1_BEGIN: &[u8] = b"TAG";
    const ID3V2_BEGIN: &[u8] = b"ID3";
    const LYRICS3_BEGIN: &[u8] = b"LYRICSBEGIN";
    const APETAG_BEGIN: &[u8] = b"APETAGEX";

    // check if at least 10 bytes are available
    if max_size < 10 {
        return 0;
    }

    let size: i32;
    if id3tag.len() >= 3 && &id3tag[0..3] == ID3V2_BEGIN {
        // ID3v2 tag -> find out size, start with 10
        let mut s = 10;
        // check for unsynchronization
        let unsynchronized = ((id3tag[5] >> 7) & 0x1) == 1;
        // check for footer
        s += if ((id3tag[5] >> 4) & 0x1) == 1 { 10 } else { 0 };
        // calculate size
        s += (id3tag[9] as i32) << 0;
        s += (id3tag[8] as i32) << 7;
        s += (id3tag[7] as i32) << 14;
        s += (id3tag[6] as i32) << 21;
        // check if size is ok
        if s > max_size {
            return 0;
        }
        // pay attention to unsynchronization where needed
        if unsynchronized {
            let mut p = 0;
            while p < s - 1 {
                if id3tag[p as usize] == 0xFF && id3tag[p as usize + 1] == 0x00 {
                    p += 1;
                    s += 1;
                    if s > max_size {
                        return 0;
                    }
                }
                p += 1;
            }
        }
        size = s;
    } else if id3tag.len() >= 3 && &id3tag[0..3] == ID3V1_BEGIN {
        // ID3v1 tag -> size is exactly 128 byte
        size = 128;
        // check if size is ok
        if size > max_size {
            return 0;
        }
    } else if id3tag.len() >= 6 && id3tag[0..6] == LYRICS3_BEGIN[..6] && max_size >= 148 {
        // LYRICS3 tag, must be followed by ID3v1 -> doublecheck and find out size
        let tail = (max_size - 128) as usize;
        if id3tag.len() >= 11
            && &id3tag[0..11] == LYRICS3_BEGIN
            && id3tag.len() >= tail + 3
            && &id3tag[tail..tail + 3] == ID3V1_BEGIN
            && id3tag.len() >= tail + 9 + 6
            && id3tag[tail + 9..tail + 9 + 6] == LYRICS3_BEGIN[..6]
        {
            size = max_size;
        } else {
            return 0;
        }
    } else if id3tag.len() >= 8 && &id3tag[0..8] == APETAG_BEGIN {
        // APE tag -> keep all the data after (ok solution for now)
        size = max_size;
    } else {
        // no tag header found -> not a proper tag
        return 0;
    }

    size
}

/// calculate frame crc
pub fn mp3_calc_layer3_crc(header: &[u8], sideinfo: &[u8], sidesize: i32) -> u16 {
    // crc has a start value of 0xFFFF
    let mut crc: u16 = 0xFFFF;

    // process two last bytes from header...
    crc = (crc << 8) ^ CRC_TABLE[((crc >> 8) ^ header[2] as u16) as usize];
    crc = (crc << 8) ^ CRC_TABLE[((crc >> 8) ^ header[3] as u16) as usize];
    // ... and all the bytes from the side information
    for i in 0..sidesize as usize {
        crc = (crc << 8) ^ CRC_TABLE[((crc >> 8) ^ sideinfo[i] as u16) as usize];
    }

    crc
}

/// decode one MP3 frame; returns per-channel, per-granule data
pub fn mp3_decode_frame(
    st: &mut PackState,
    dec: &mut HuffmanReader,
    frame: usize,
) -> [[GranuleData; 2]; 2] {
    let mut fd: [[GranuleData; 2]; 2] = std::array::from_fn(|_| {
        std::array::from_fn(|_| GranuleData {
            scalefactors: vec![0u8; 36],
            coefficients: vec![0i16; 578],
        })
    });

    let nch = st.frames[frame].nchannels as usize;
    let mut bitp: i32 = 0;

    // decode frame
    for gr in 0..2 {
        for ch in 0..nch {
            // --- preparations ---
            let granule = st.frames[frame].granule(st, ch, gr).clone();
            let coefs = &mut fd[ch][gr].coefficients;
            // clear memory (= set coefficients/scalefactors zero)
            coefs[..576].fill(0);
            let mut scfs = vec![0u8; 36];
            // set position in stream
            dec.setpos(st.frames[frame].main_index + (bitp / 8), 8 - (bitp % 8));
            dec.reset_counter();
            // take count of inner frame main data bits
            let lmaxp = granule.main_data_bit as i32;
            bitp += lmaxp;
            // set decoding parameters
            let slen = SLENGTH_TABLE[granule.slength as usize];

            // --- scale factors ---
            let mut scfs_pos = 0usize;
            // read long block scalefactors (with sharing)
            if granule.block_type != SHORT_BLOCK as i8 {
                // get sharing params if any
                let share: i32 = if gr != 0 {
                    st.frames[frame].granule(st, ch, 0).share as i32
                } else {
                    0
                };
                // read 21 (max) scalefactors
                let mut g = 0;
                let mut p = 0;
                while g < 4 {
                    if ((share >> (3 - g)) & 0x1) != 0 {
                        // shared
                        let w = SCF_WIDTH[g] as usize;
                        let src = fd[ch][0].scalefactors[p as usize..p as usize + w].to_vec();
                        scfs[scfs_pos..scfs_pos + w].copy_from_slice(&src);
                        scfs_pos += w;
                        p = SCF_BOUNDS[g];
                    } else {
                        // non shared
                        let sl = slen[if g < 2 { 0 } else { 1 }];
                        while p < SCF_BOUNDS[g] {
                            scfs[scfs_pos] = dec.read_bits(sl) as u8;
                            scfs_pos += 1;
                            p += 1;
                        }
                    }
                    g += 1;
                }
            } else {
                // read short block scfs (no sharing!)
                for _ in 0..3 {
                    // 3 subblocks
                    let mut g = 0;
                    let mut p = 0;
                    while g < 3 {
                        // 3 groups
                        let sl = slen[if g == 0 { 0 } else { 1 }];
                        while p < SCF_BOUNDS_SHORT[g] {
                            scfs[scfs_pos] = dec.read_bits(sl) as u8;
                            scfs_pos += 1;
                            p += 1;
                        }
                        g += 1;
                    }
                }
            }

            // --- coefficients / big values ---
            let mut p: i32 = 0;
            let mut r = 0;
            let mut cf_pos: usize = 0;
            while r < 3 {
                if granule.region_table[r] == 0 {
                    // tbl0 skipping
                    cf_pos = granule.region_bound[r] as usize;
                    p = granule.region_bound[r] as i32;
                    r += 1;
                    continue;
                }
                // decoding with other tables
                let bv_table = &BV_DEC_TABLE[granule.region_table[r] as usize];
                if bv_table.h.is_none() {
                    return fd;
                }
                let conv_set: &[HuffmanConvSet] = bv_table.h.unwrap();
                let linbits = bv_table.linbits as i32;
                if linbits == 0 {
                    // without linbits
                    while p < granule.region_bound[r] as i32 {
                        let mut vals = [0u8; 2];
                        dec.decode_pair(conv_set, &mut vals);
                        for i in 0..2 {
                            if vals[i] > 0 {
                                coefs[cf_pos + i] =
                                    if dec.read_bit() != 0 { -(vals[i] as i16) } else { vals[i] as i16 };
                            }
                        }
                        if dec.get_count() > lmaxp {
                            // high error tolerance!
                            coefs[cf_pos..cf_pos + 2].fill(0);
                            break;
                        }
                        p += 2;
                        cf_pos += 2;
                    }
                } else {
                    // with linbits
                    while p < granule.region_bound[r] as i32 {
                        let mut vals = [0u8; 2];
                        dec.decode_pair(conv_set, &mut vals);
                        for i in 0..2 {
                            if vals[i] > 0 {
                                coefs[cf_pos + i] = if vals[i] == 15 {
                                    15 + dec.read_bits(linbits) as i16
                                } else {
                                    vals[i] as i16
                                };
                                if dec.read_bit() != 0 {
                                    coefs[cf_pos + i] = -coefs[cf_pos + i];
                                }
                            }
                        }
                        if dec.get_count() > lmaxp {
                            // high error tolerance!
                            coefs[cf_pos..cf_pos + 2].fill(0);
                            break;
                        }
                        p += 2;
                        cf_pos += 2;
                    }
                }
                r += 1;
            }

            // --- coefficients / small values ---
            let conv_set: &HuffmanConvSet = if granule.select_htabb != 0 {
                &HTABB_DEC
            } else {
                &HTABA_DEC
            };
            while (dec.get_count() < lmaxp) && (p < 576) {
                let mut vals = [0u8; 4];
                dec.decode_quadruple(conv_set, &mut vals);
                for i in 0..4 {
                    if vals[i] != 0 {
                        coefs[cf_pos + i] = if dec.read_bit() != 0 { -1 } else { 1 };
                    }
                }
                if dec.get_count() > lmaxp {
                    coefs[cf_pos..cf_pos + 4].fill(0);
                    break;
                }
                p += 4;
                cf_pos += 4;
            }

            // set sv_bound
            {
                let gidx = st.frames[frame].granules[ch][gr];
                st.granules[gidx].sv_bound = p as i16;
            }
            fd[ch][gr].scalefactors.copy_from_slice(&scfs);
        }
    }

    fd
}
