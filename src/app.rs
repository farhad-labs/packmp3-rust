// app.rs - application level processing routines.
// Faithful port of the C++ functions reset_buffers(), read_mp3(),
// analyze_frames() and write_mp3() in source/packmp3.cpp.
// The C++ code uses module-level globals (str_in/str_out and friends); here the
// streams are passed explicitly and all state lives in `PackState`.

use crate::bitops::{AbyteWriter, IoStream};
use crate::pmp::frame_chain;
use crate::state::{AnalysisInfo, PackState, EL_FATAL};
use crate::tables::{
    FIRST_FRAME_AREA, GARBAGE_TOLERANCE, MAX_BAD_FIRST, MP3_MONO, FILETYPE_DESCRIPTION,
    mp3_bitrate_table, mp3_samplerate_table,
};
use crate::mp3;

/// `MBITS(c, l, r)` from bitops.h: keep `l` low bits, shift right by `r`.
fn mbits(c: u8, l: i32, r: i32) -> i32 {
    ((c as i32) & (0xFF >> (8 - l))) >> r
}

/// MPEG1_LAYER_III type index (pmp3tbl.h)
const MPEG1_LAYER_III: i32 = 0xD;

/// error message mirroring C++ `MEM_ERRMSG`
const MEM_ERRMSG: &str = "out of memory error";

/// set each variable to its initial value
pub fn reset_buffers(st: &mut PackState) -> bool {
    // free frame / granule data (cleared wholesale in the arena model)
    st.firstframe = None;
    st.lastframe = None;
    st.frames.clear();
    st.granules.clear();

    // throw away huffman coded data block
    st.main_data.clear();
    st.main_data_size = 0;

    // free other data
    st.data_before.clear();
    st.data_after.clear();
    st.unmute_data.clear();
    st.data_before_size = 0;
    st.data_after_size = 0;
    st.unmute_data_size = 0;
    st.gg_context = [None, None];

    // reset global variables
    st.g_nframes = 0;
    st.g_nchannels = 0;
    st.g_samplerate = 0;
    st.g_bitrate = 0;
    st.n_bad_first = 0;

    // reset frame analysis variables (all -1)
    st.ai = AnalysisInfo::default();

    true
}

/// parse MP3 frame structure
pub fn read_mp3(st: &mut PackState, str_in: &mut IoStream) -> bool {
    let mut incomplete_last_frame = false;

    let mut mpeg: i32 = -1;
    let mut layer: i32 = -1;
    let mut samples: i32 = -1;
    let mut channels: i32 = -1;

    // read the first few bytes from the file into memory
    // (these will be used to check for proper frames)
    st.mp3filesize = str_in.getsize() as i32;
    if st.mp3filesize > FIRST_FRAME_AREA {
        st.mp3filesize = FIRST_FRAME_AREA;
    }
    if st.mp3filesize <= 0 {
        st.errormessage = MEM_ERRMSG.to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }
    let mut mp3data = vec![0u8; (st.mp3filesize + 1) as usize];
    str_in.read(&mut mp3data, 1, st.mp3filesize);

    // find first proper mpeg audio frame
    let mut pos = mp3::mp3_seek_firstframe(&mp3data, st.mp3filesize);
    if pos == -1 {
        st.errormessage = "no mpeg audio data recognized".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }
    let main_data_begin = pos;

    // check first frame header for MPEG and LAYER version
    if st.mp3filesize - pos >= 2 {
        let type_v = mbits(mp3data[(pos + 1) as usize], 5, 1);
        if type_v != MPEG1_LAYER_III {
            let d = FILETYPE_DESCRIPTION
                .get(type_v as usize)
                .copied()
                .unwrap_or("not an MPEG audio file");
            st.errormessage = format!("file is {}, not supported", d);
            st.errorlevel = EL_FATAL;
            return false;
        }
    }

    // if theres still more data, read the whole file into memory now
    // (bad for mem consumption, but for now -> so what?)
    if st.mp3filesize == FIRST_FRAME_AREA {
        st.mp3filesize = str_in.getsize() as i32; // check size of file again
        mp3data.resize((st.mp3filesize + 1) as usize, 0);
        str_in.read(
            &mut mp3data[FIRST_FRAME_AREA as usize..],
            1,
            st.mp3filesize - FIRST_FRAME_AREA,
        );
    }

    // now, read frames until the end of the stream or a bad frame is encountered
    let mut data_writer = AbyteWriter::new(0);
    while pos < st.mp3filesize {
        // read frame, check result
        let fi = match mp3::mp3_read_frame(
            st,
            &mp3data[pos as usize..],
            st.mp3filesize - pos,
        ) {
            Some(f) => f,
            None => {
                if st.errorlevel == EL_FATAL {
                    // append some useful info to the existing error message
                    let n = match st.lastframe {
                        Some(l) => st.frames[l].n + 1,
                        None => 0,
                    };
                    st.errormessage =
                        format!("{} (frame #{} at 0x{:X})", st.errormessage, n, pos);
                    return false;
                }
                break;
            }
        };
        let frame_size = st.frames[fi].frame_size;
        if frame_size > st.mp3filesize - pos {
            // check for incomplete last frame
            mp3::mp3_discard_frame(st, fi);
            incomplete_last_frame = true;
            break;
        }
        let (fmpeg, flayer, fsamples, fchannels) = {
            let f = &st.frames[fi];
            (f.mpeg as i32, f.layer as i32, f.samples as i32, f.channels as i32)
        };
        if (fmpeg != mpeg) || (flayer != layer) || (fsamples != samples) || (fchannels != channels) {
            // check for inconsistencies
            if mpeg == -1 {
                mpeg = fmpeg;
                layer = flayer;
                samples = fsamples;
                channels = fchannels;
            } else {
                mp3::mp3_discard_frame(st, fi);
                break;
            }
        }
        if let Some(last) = st.lastframe {
            // fix previous frames aux size, check for problems
            let (aux_size, bit_res, n_is_neg1) = {
                let f = &st.frames[fi];
                (f.aux_size, f.bit_reservoir as i32, f.n == -1)
            };
            if aux_size < 0 {
                // main data out of bounds
                if st.frames[last].n > MAX_BAD_FIRST - 2 {
                    mp3::mp3_discard_frame(st, fi);
                    break;
                } else {
                    st.n_bad_first = st.frames[last].n + 2;
                }
            } else if st.frames[last].aux_size < bit_res {
                // overlapping main data
                if st.frames[last].n > MAX_BAD_FIRST - 1 {
                    mp3::mp3_discard_frame(st, fi);
                    break;
                } else {
                    st.n_bad_first = st.frames[last].n + 1;
                }
            } else if n_is_neg1 {
                // obvious contradiction (see mp3_read_frame())
                if st.frames[last].n > MAX_BAD_FIRST - 2 {
                    mp3::mp3_discard_frame(st, fi);
                    break;
                } else {
                    st.n_bad_first = st.frames[last].n + 2;
                }
            }
            // fix previous frame aux size
            st.frames[last].aux_size -= bit_res;
        } else if (st.frames[fi].aux_size < 0) || (st.frames[fi].n == -1) {
            st.n_bad_first = 1;
        }
        // update main index, store main data
        let bit_res = st.frames[fi].bit_reservoir as i32;
        st.frames[fi].main_index = data_writer.getpos() as i32 - bit_res;
        let fixed_size = st.frames[fi].fixed_size;
        let auxlen = (frame_size - fixed_size) as usize;
        let start = (pos + fixed_size) as usize;
        data_writer.write_n(&mp3data[start..start + auxlen]);
        // insert frame into the frame chain
        mp3::mp3_append_frame(st, fi);
        // check for pointing to empty space
        if st.frames[fi].main_index < 0 {
            st.n_bad_first = st.frames[fi].n + 1;
        }
        // advance to next physical frame
        pos += frame_size;
    }

    // store main data
    st.main_data_size = data_writer.getpos() as i32;
    if st.main_data_size > 0 {
        st.main_data = data_writer.getptr();
    }

    // check number of proper frames (must be at least 5)
    match st.lastframe {
        Some(last) => {
            if st.frames[last].n + 1 - st.n_bad_first < 5 {
                st.errormessage = "corrupted file, compression not possible".to_string();
                st.errorlevel = EL_FATAL;
                return false;
            }
        }
        None => {
            st.errormessage = "corrupted file, compression not possible".to_string();
            st.errorlevel = EL_FATAL;
            return false;
        }
    }

    // end of main mp3 data processing
    let main_data_end = pos;
    // check for ID3 tags at end of file
    let id3_ok = mp3::mp3_get_id3_size(
        &mp3data[main_data_end as usize..],
        st.mp3filesize - main_data_end,
    );
    if !incomplete_last_frame
        && (main_data_end < st.mp3filesize)
        && (main_data_end + id3_ok != st.mp3filesize)
        && (st.mp3filesize - main_data_end > GARBAGE_TOLERANCE)
    {
        // allow for a tolerance of 64K unidentified garbage data at EOF
        st.errormessage = format!(
            "synching failure (frame #{} at 0x{:X})",
            st.frames[st.lastframe.unwrap()].n + 1,
            pos
        );
        st.errorlevel = EL_FATAL;
        return false;
    }

    // clean up and store id3 tags and garbage data
    st.data_after_size = st.mp3filesize - main_data_end;
    st.data_before_size = main_data_begin;
    if st.data_after_size > 0 {
        st.data_after = mp3data[main_data_end as usize..].to_vec();
    }
    if st.data_before_size > 0 {
        st.data_before = mp3data[..main_data_begin as usize].to_vec();
        st.data_before.truncate(st.data_before_size as usize);
    }

    true
}

/// analyse frames for redundancies and errors
pub fn analyze_frames(st: &mut PackState) -> bool {
    let first = st.firstframe;
    let mut aux0 = false;
    let mut aux1 = false;
    let mut sbx_gain;

    // (re)set analysis data
    {
        let ai = &mut st.ai;
        let f = &st.frames[first.unwrap()];
        ai.i_mpeg = f.mpeg;
        ai.i_layer = f.layer;
        ai.i_samplerate = f.samples;
        ai.i_bitrate = f.bits;
        ai.i_protection = f.protection;
        ai.i_padding = f.padding;
        ai.i_privbit = f.privbit;
        ai.i_channels = f.channels;
        ai.i_stereo_ms = f.stereo_ms;
        ai.i_stereo_int = f.stereo_int;
        ai.i_copyright = f.copyright;
        ai.i_original = f.original;
        ai.i_emphasis = f.emphasis;
        ai.i_padbits = f.padbits;
        ai.i_bit_res = if f.bit_reservoir > 0 { 1 } else { 0 };
        if f.aux_size > 0 {
            ai.i_aux_h = 1;
            aux0 = true;
        } else {
            ai.i_aux_h = 0;
        }
        // for granules
        let gidx = f.granules[0][0];
        let granule = &st.granules[gidx];
        ai.i_share = if granule.share != 0 { 1 } else { 0 };
        ai.i_sblocks = if granule.window_switching != 0 { 1 } else { 0 };
        ai.i_mixed = if granule.mixed_flag != 0 { 1 } else { 0 };
        ai.i_preemphasis = granule.preemphasis;
        ai.i_coarse = granule.coarse_scalefactors;
        // easy way out for these - will be tested later
        ai.i_sb_diff = 0;
        ai.i_sbgain = 0;
    }

    // analyse frames, check for redundancies and errors
    let mut fi = st.firstframe;
    while let Some(fidx) = fi {
        let n = st.frames[fidx].n;
        // no need to check mpeg, layer, samples, channels
        // already did this in read_mp3()!

        // analyse (mainly) frame header data
        // special tolerance for the following, as they often get mixed up in frame #0
        if (st.ai.i_privbit != -1) && (st.ai.i_privbit != st.frames[fidx].privbit) {
            if n == 1 {
                // tolerance for first frame
                if st.n_bad_first == 0 {
                    st.n_bad_first = 1;
                }
                st.ai.i_privbit = st.frames[fidx].privbit;
            } else {
                st.ai.i_privbit = -1;
            }
        }
        if (st.ai.i_copyright != -1) && (st.ai.i_copyright != st.frames[fidx].copyright) {
            if n == 1 {
                // tolerance for first frame
                if st.n_bad_first == 0 {
                    st.n_bad_first = 1;
                }
                st.ai.i_copyright = st.frames[fidx].copyright;
            } else {
                st.ai.i_copyright = -1;
            }
        }
        if (st.ai.i_original != -1) && (st.ai.i_original != st.frames[fidx].original) {
            if n == 1 {
                // tolerance for first frame
                if st.n_bad_first == 0 {
                    st.n_bad_first = 1;
                }
                st.ai.i_original = st.frames[fidx].original;
            } else {
                st.ai.i_original = -1;
            }
        }
        if (st.ai.i_protection != -1) && (st.ai.i_protection != st.frames[fidx].protection) {
            st.ai.i_protection = -1;
        }
        if (st.ai.i_bitrate != -1) && (st.ai.i_bitrate != st.frames[fidx].bits) {
            st.ai.i_bitrate = -1;
        }
        if (st.ai.i_padding != -1) && (st.ai.i_padding != st.frames[fidx].padding) {
            st.ai.i_padding = -1;
        }
        if (st.ai.i_stereo_ms != -1) && (st.ai.i_stereo_ms != st.frames[fidx].stereo_ms) {
            st.ai.i_stereo_ms = -1;
        }
        if (st.ai.i_stereo_int != -1) && (st.ai.i_stereo_int != st.frames[fidx].stereo_int) {
            st.ai.i_stereo_int = -1;
        }
        if (st.ai.i_emphasis != -1) && (st.ai.i_emphasis != st.frames[fidx].emphasis) {
            st.ai.i_emphasis = -1;
        }
        if (st.ai.i_padbits != -1) && (st.ai.i_padbits != st.frames[fidx].padbits) {
            st.ai.i_padbits = -1;
        }
        if (st.ai.i_bit_res != 1) && (st.frames[fidx].bit_reservoir > 0) {
            st.ai.i_bit_res = 1;
        }
        if st.ai.i_aux_h != -1 {
            // no auxiliary before aux0 or after aux1
            if st.frames[fidx].aux_size > 0 {
                if !aux0 {
                    aux1 = true;
                }
            } else {
                if aux1 {
                    st.ai.i_aux_h = -1;
                }
                if aux0 {
                    aux0 = false;
                }
            }
        }
        let nch = st.frames[fidx].nchannels as usize;
        for ch in 0..nch {
            for gr in 0..2 {
                let gidx = st.frames[fidx].granules[ch][gr];
                sbx_gain = (st.granules[gidx].sb_gain[0] != 0)
                    || (st.granules[gidx].sb_gain[1] != 0)
                    || (st.granules[gidx].sb_gain[2] != 0);
                if (st.ai.i_share != 1) && (st.granules[gidx].share != 0) {
                    st.ai.i_share = 1;
                }
                if (st.ai.i_sblocks != 1) && (st.granules[gidx].window_switching != 0) {
                    st.ai.i_sblocks = 1;
                }
                if (st.ai.i_mixed != 1) && (st.granules[gidx].mixed_flag != 0) {
                    st.ai.i_mixed = 1;
                }
                if (st.ai.i_preemphasis != -1)
                    && (st.ai.i_preemphasis != st.granules[gidx].preemphasis)
                {
                    st.ai.i_preemphasis = -1;
                }
                if (st.ai.i_coarse != -1) && (st.ai.i_coarse != st.granules[gidx].coarse_scalefactors)
                {
                    st.ai.i_coarse = -1;
                }
                if (st.ai.i_sbgain != -1) && sbx_gain {
                    st.ai.i_sbgain = if st.granules[gidx].block_type == crate::tables::SHORT_BLOCK as i8
                    {
                        1
                    } else {
                        -1
                    };
                }
                if (st.ai.i_sb_diff != 1)
                    && (ch == 1)
                    && (st.granules[gidx].block_type
                        != st.granules[st.frames[fidx].granules[0][gr]].block_type)
                {
                    st.ai.i_sb_diff = 1;
                }
            }
        }
        fi = st.frames[fidx].next;
    }

    // last check for aux data handling
    if (st.ai.i_aux_h != -1) && aux1 {
        st.ai.i_aux_h = 1;
    }

    // fill in some global info
    st.g_nframes = st.frames[st.lastframe.unwrap()].n + 1;
    st.g_nchannels = if st.ai.i_channels == MP3_MONO as i8 { 1 } else { 2 };
    st.g_samplerate = mp3_samplerate_table(st.ai.i_samplerate as i32);
    st.g_bitrate = if st.ai.i_bitrate == -1 {
        0
    } else {
        mp3_bitrate_table(st.ai.i_bitrate as i32)
    };

    true
}

/// write MP3 from frame structure
pub fn write_mp3(st: &mut PackState, str_out: &mut IoStream) -> bool {
    // write data before (usually ID3v2 tag)
    str_out.write(&st.data_before, 1, st.data_before_size);

    // write physical frames
    // main data has to be in order!
    let chain = frame_chain(st);
    let mut data_idx: usize = 0;
    for &fi in &chain {
        let fixed = mp3::mp3_build_fixed(st, fi);
        let f = &st.frames[fi];
        str_out.write(&fixed, 1, f.fixed_size);
        let auxlen = (f.frame_size - f.fixed_size) as usize;
        str_out.write(&st.main_data[data_idx..data_idx + auxlen], 1, auxlen as i32);
        data_idx += auxlen;
    }

    // write data after
    str_out.write(&st.data_after, 1, st.data_after_size);

    // errormessage if write error
    if str_out.chkerr() {
        st.errormessage = "write error, possibly drive is full".to_string();
        st.errorlevel = EL_FATAL;
        return false;
    }

    // get filesize
    st.mp3filesize = str_out.getsize() as i32;

    true
}
