// state.rs - shared data structures and global state for the packMP3 port.
//
// The C++ code keeps the frame/granule chains and all processing globals as
// module-level variables and links the structures with raw pointers. We model
// the linked lists with index-based arenas (`frames` / `granules`) owned by a
// single `PackState`, which plays the role of the C++ globals.

// error levels
pub const EL_WRONG_INPUT: i32 = -1;
pub const EL_NO_ERROR: i32 = 0;
pub const EL_WARNING: i32 = 1;
pub const EL_FATAL: i32 = 2;

// actions
pub const A_COMPRESS: i32 = 1;
pub const A_DUMP_SEPERATE: i32 = 2;
pub const A_FILE_ANALYSIS: i32 = 3;
pub const A_BLOCK_ANALYSIS: i32 = 4;
pub const A_STATS_ANALYSIS: i32 = 5;
pub const A_PGM_INFO: i32 = 6;

pub const MSG_SIZE: usize = 128;
pub const BARLEN: i32 = 36;

// ---------- granule data (decode result buffers) ----------

pub struct GranuleData {
    pub scalefactors: Vec<u8>,   // 36 bytes
    pub coefficients: Vec<i16>,  // 578 entries
}

// ---------- granule info (side-info) ----------

#[derive(Clone, Default)]
pub struct GranuleInfo {
    // # of current granule (can be calculated from frame->n)
    pub n: i32,
    // universal data
    pub share: i8,
    pub main_data_bit: i16,
    pub big_val_pairs: i16,
    pub global_gain: i16,
    pub slength: i8,
    pub window_switching: i8,
    pub preemphasis: i8,
    pub coarse_scalefactors: i8,
    pub select_htabb: i8,
    // info specific to normal blocks
    pub region_table: [i8; 3],
    pub region0_size: i8,
    pub region1_size: i8,
    pub region_bound: [i16; 3],
    pub sv_bound: i16,
    // info specific to special blocks
    pub block_type: i8,
    pub mixed_flag: i8,
    pub sb_gain: [i8; 3],
    // links to next & previous granule (arena indices)
    pub next: Option<usize>,
    pub prev: Option<usize>,
}

// ---------- mp3 frame ----------

#[derive(Clone, Default)]
pub struct Frame {
    // # of current frame
    pub n: i32,
    // information contained in header
    pub mpeg: i8,
    pub layer: i8,
    pub protection: i8,
    pub bits: i8,
    pub samples: i8,
    pub padding: i8,
    pub privbit: i8,
    pub channels: i8,
    pub stereo_ms: i8,
    pub stereo_int: i8,
    pub copyright: i8,
    pub original: i8,
    pub emphasis: i8,
    // information contained in side info
    pub bit_reservoir: i16,
    pub padbits: i8,
    // granule arena indices, one pair per channel: granules[ch][gr]
    pub granules: Vec<[usize; 2]>,
    // additional info
    pub frame_size: i32,
    pub fixed_size: i32,
    pub main_size: i32,
    pub aux_size: i32,
    pub main_index: i32,
    pub main_bits: i32,
    pub nchannels: i8,
    // links to next & previous frame (arena indices)
    pub next: Option<usize>,
    pub prev: Option<usize>,
}

impl Frame {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn granule<'s>(&self, st: &'s PackState, ch: usize, gr: usize) -> &'s GranuleInfo {
        let idx = self.granules[ch][gr];
        &st.granules[idx]
    }

    pub fn granule_mut<'s>(&self, st: &'s mut PackState, ch: usize, gr: usize) -> &'s mut GranuleInfo {
        let idx = self.granules[ch][gr];
        &mut st.granules[idx]
    }
}

// ---------- frame analysis info ----------

#[derive(Clone)]
pub struct AnalysisInfo {
    pub i_mpeg: i8,
    pub i_layer: i8,
    pub i_samplerate: i8,
    pub i_bitrate: i8,
    pub i_protection: i8,
    pub i_padding: i8,
    pub i_privbit: i8,
    pub i_channels: i8,
    pub i_stereo_ms: i8,
    pub i_stereo_int: i8,
    pub i_copyright: i8,
    pub i_original: i8,
    pub i_emphasis: i8,
    pub i_padbits: i8,
    pub i_bit_res: i8,
    pub i_share: i8,
    pub i_sblocks: i8,
    pub i_mixed: i8,
    pub i_preemphasis: i8,
    pub i_coarse: i8,
    pub i_sbgain: i8,
    pub i_aux_h: i8,
    pub i_sb_diff: i8,
}

impl Default for AnalysisInfo {
    fn default() -> Self {
        AnalysisInfo {
            i_mpeg: -1,
            i_layer: -1,
            i_samplerate: -1,
            i_bitrate: -1,
            i_protection: -1,
            i_padding: -1,
            i_privbit: -1,
            i_channels: -1,
            i_stereo_ms: -1,
            i_stereo_int: -1,
            i_copyright: -1,
            i_original: -1,
            i_emphasis: -1,
            i_padbits: -1,
            i_bit_res: -1,
            i_share: -1,
            i_sblocks: -1,
            i_mixed: -1,
            i_preemphasis: -1,
            i_coarse: -1,
            i_sbgain: -1,
            i_aux_h: -1,
            i_sb_diff: -1,
        }
    }
}

// ---------- LAME ancillary data predictor state ----------
//
// pmp_predict_lame_anc() keeps mutable state between calls (the C++ code uses
// function-local `static` variables). We store it here so the port needs no
// unsafe statics; pmp.rs manipulates it through `PackState`.

#[derive(Clone)]
pub struct LameAncPredictor {
    /// prediction buffer (2048 bytes)
    pub pred: Vec<u8>,
    /// 'LAME...' extension string (4 + 16 bytes)
    pub lame_str: Vec<u8>,
    pub lame_str_len: i32,
    pub lame_bit: i32,
    pub alt_pred: bool,
}

impl Default for LameAncPredictor {
    fn default() -> Self {
        LameAncPredictor {
            pred: vec![0u8; 2048],
            lame_str: vec![0u8; 20],
            lame_str_len: 4,
            lame_bit: 0,
            alt_pred: false,
        }
    }
}

// ---------- global processing state ----------

#[derive(Clone, Default)]
pub struct PackState {
    // frame chain
    pub firstframe: Option<usize>,
    pub lastframe: Option<usize>,
    pub frames: Vec<Frame>,
    pub granules: Vec<GranuleInfo>,
    // data storage
    pub main_data: Vec<u8>,
    pub data_before: Vec<u8>,
    pub data_after: Vec<u8>,
    pub unmute_data: Vec<u8>,
    pub main_data_size: i32,
    pub data_before_size: i32,
    pub data_after_size: i32,
    pub unmute_data_size: i32,
    pub n_bad_first: i32,
    pub gg_context: [Option<Vec<u8>>; 2],
    // LAME ancillary data predictor state (see above)
    pub anc: LameAncPredictor,
    // info about audio file
    pub g_nframes: i32,
    pub g_nchannels: i32,
    pub g_samplerate: i32,
    pub g_bitrate: i32,
    // frame analysis info
    pub ai: AnalysisInfo,
    // error handling
    pub errormessage: String,
    pub errorlevel: i32,
    // file info
    pub mp3filesize: i32,
    pub pmpfilesize: i32,
    pub filetype: i32,
}

impl PackState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn alloc_granule(&mut self, g: GranuleInfo) -> usize {
        self.granules.push(g);
        self.granules.len() - 1
    }

    pub fn alloc_frame(&mut self, f: Frame) -> usize {
        self.frames.push(f);
        self.frames.len() - 1
    }
}

/// size of one unmute record for a given channel count
pub fn unmute_record_size(nch: i32) -> i32 {
    2 + (nch * 2 * 4)
}
