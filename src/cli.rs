// cli.rs - command line interface for the packmp3 binary.
//
// Faithful port of the non-library, non-DEV UI parts of source/packmp3.cpp:
// main(), initialize_options(), process_ui(), check_file(), process_file(),
// execute(), get_status(), show_help(), swap_streams(), compare_output() and
// the filename helper functions (create_filename, unique_filename,
// set_extension, add_underscore, file_exists, progress_bar).
//
// The C++ code uses a sea of module-level globals; here all mutable state
// lives in the `Cli` struct and the message stream is an owned `Write`.

use std::io::{self, Write};
use std::time::Instant;

use crate::app;
use crate::bitops::{IoSource, IoStream, MODE_READ, MODE_WRITE, TYPE_FILE, TYPE_MEMORY, TYPE_STREAM};
use crate::pmp::{self, APP_VERSION, PMP_MAGIC};
use crate::state::{A_COMPRESS, BARLEN, EL_FATAL, EL_NO_ERROR, EL_WARNING, PackState};
use crate::tables::{F_MP3, F_PMP, F_UNK};

// ---------------------------------------------------------------------------
// program constants (mirror packmp3.cpp)
// ---------------------------------------------------------------------------

pub const SUBVERSION: &str = "g";
pub const APPTITLE: &str = "packMP3";
pub const APPNAME: &str = "packMP3";
pub const VERSIONDATE: &str = "01/22/2016";
pub const AUTHOR: &str = "Matthias Stirner";
pub const COPYRIGHT: &str = "2010-2016 Ratisbon University & Matthias Stirner";
pub const PMP_EXT: &str = "pmp";
pub const MP3_EXT: &str = "mp3";
pub const WEBSITE: &str = "http://packjpg.encode.ru/";
pub const EMAIL: &str = "packjpg (at) matthiasstirner.com";
const FRD_ERRMSG: &str = "could not read file / file not found";
const FWR_ERRMSG: &str = "could not write file / file write-protected";

// ---------------------------------------------------------------------------
// function identifiers (C++ uses function pointers for this)
// ---------------------------------------------------------------------------

/// the processing function that is responsible for the current error/warning
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorFunction {
    CheckFile,
    ReadMp3,
    WriteMp3,
    AnalyzeFrames,
    CompressMp3,
    UncompressPmp,
    SwapStreams,
    CompareOutput,
    ResetBuffers,
}

impl ErrorFunction {
    /// status message, mirrors C++ get_status()
    pub fn status(self) -> &'static str {
        match self {
            ErrorFunction::CheckFile => "Determining filetype",
            ErrorFunction::ReadMp3 => "Reading MP3",
            ErrorFunction::WriteMp3 => "Writing MP3",
            ErrorFunction::AnalyzeFrames => "Analysing frames",
            ErrorFunction::CompressMp3 => "Compressing to PMP",
            ErrorFunction::UncompressPmp => "Uncompressing PMP",
            ErrorFunction::SwapStreams => "Swapping input/output streams",
            ErrorFunction::CompareOutput => "Verifying output stream",
            ErrorFunction::ResetBuffers => "Resetting program",
        }
    }

    /// invoke the underlying processing function (mirrors calling the
    /// function pointer in C++).
    fn run(self, cli: &mut Cli) -> bool {
        match self {
            ErrorFunction::CheckFile => cli.check_file(),
            ErrorFunction::ReadMp3 => app::read_mp3(&mut cli.st, cli.str_in.as_mut().unwrap()),
            ErrorFunction::WriteMp3 => app::write_mp3(&mut cli.st, cli.str_out.as_mut().unwrap()),
            ErrorFunction::AnalyzeFrames => app::analyze_frames(&mut cli.st),
            ErrorFunction::CompressMp3 => {
                pmp::compress_mp3(&mut cli.st, cli.str_out.as_mut().unwrap())
            }
            ErrorFunction::UncompressPmp => {
                pmp::uncompress_pmp(&mut cli.st, cli.str_in.as_mut().unwrap())
            }
            ErrorFunction::SwapStreams => cli.swap_streams(),
            ErrorFunction::CompareOutput => cli.compare_output(),
            ErrorFunction::ResetBuffers => app::reset_buffers(&mut cli.st),
        }
    }
}

// ---------------------------------------------------------------------------
// Cli - holds all CLI state
// ---------------------------------------------------------------------------

pub struct Cli {
    // file list & position
    pub filelist: Vec<String>,
    pub file_cnt: usize,
    pub file_no: usize,
    // options
    pub verify_lv: i32,
    pub err_tol: i32,
    pub verbosity: i32,
    pub overwrite: bool,
    pub wait_exit: bool,
    pub pipe_on: bool,
    pub action: i32,
    // message stream
    pub msgout: Box<dyn Write>,
    pub msgout_is_stderr: bool,
    // shared codec state
    pub st: PackState,
    // per-file processing state
    pub mp3filename: String,
    pub pmpfilename: String,
    pub str_in: Option<IoStream>,
    pub str_out: Option<IoStream>,
    pub str_str: Option<IoStream>,
    pub errorfunction: Option<ErrorFunction>,
    // summary storage (per file)
    pub err_list: Vec<Option<String>>,
    pub err_tp: Vec<i32>,
}

impl Default for Cli {
    fn default() -> Self {
        Cli {
            filelist: Vec::new(),
            file_cnt: 0,
            file_no: 0,
            verify_lv: 0,
            err_tol: 1,
            verbosity: -1,
            overwrite: false,
            wait_exit: true,
            pipe_on: false,
            action: A_COMPRESS,
            msgout: Box::new(io::stdout()),
            msgout_is_stderr: false,
            st: PackState::new(),
            mp3filename: String::new(),
            pmpfilename: String::new(),
            str_in: None,
            str_out: None,
            str_str: None,
            errorfunction: None,
            err_list: Vec::new(),
            err_tp: Vec::new(),
        }
    }
}

impl Cli {
    pub fn new() -> Self {
        Self::default()
    }

    // -----------------------------------------------------------------------
    // main() equivalent
    // -----------------------------------------------------------------------

    /// run the whole program; returns the process exit code.
    /// `args` is the full argument list including argv[0] (program name).
    pub fn run_main(&mut self, args: &[String]) -> i32 {
        self.initialize_options(args);

        // write program info to screen
        let _ = writeln!(
            self.msgout,
            "\n--> {} v{}.{}{} ({}) by {} <--",
            APPTITLE,
            APP_VERSION / 10,
            APP_VERSION % 10,
            SUBVERSION,
            VERSIONDATE,
            AUTHOR
        );
        let _ = writeln!(self.msgout, "Copyright {}", COPYRIGHT);
        let _ = writeln!(self.msgout, "All rights reserved\n");

        // check if user input is wrong, show help screen if it is
        if (self.file_cnt == 0)
            || ((self.action != A_COMPRESS) || (self.verify_lv > 1))
        {
            self.show_help();
            return -1;
        }

        // (re)set program has to be done first
        app::reset_buffers(&mut self.st);

        // process file(s) - this is the main function routine
        let mut error_cnt = 0usize;
        let mut warn_cnt = 0usize;
        let mut acc_mp3size = 0.0f64;
        let mut acc_pmpsize = 0.0f64;

        let begin = Instant::now();
        for file_no in 0..self.file_cnt {
            // process current file
            self.file_no = file_no;
            self.process_ui();
            // store error message and type if any
            if self.st.errorlevel > 0 {
                self.err_list[file_no] = Some(self.st.errormessage.clone());
                self.err_tp[file_no] = self.st.errorlevel;
            }
            // count errors / warnings / file sizes
            if self.st.errorlevel >= self.err_tol {
                error_cnt += 1;
            } else {
                if self.st.errorlevel == EL_WARNING {
                    warn_cnt += 1;
                }
                acc_mp3size += self.st.mp3filesize as f64;
                acc_pmpsize += self.st.pmpfilesize as f64;
            }
        }
        let end = Instant::now();

        // errors summary: only needed for -v2 or progress bar
        if (self.verbosity == -1) || (self.verbosity == 2) {
            // print summary of errors to screen
            if error_cnt > 0 {
                let _ = writeln!(self.msgout, "\n\nfiles with errors:");
                let _ = writeln!(self.msgout, "------------------");
                for file_no in 0..self.file_cnt {
                    if self.err_tp[file_no] >= self.err_tol {
                        let _ = writeln!(
                            self.msgout,
                            "{} ({})",
                            self.filelist[file_no],
                            self.err_list[file_no].as_deref().unwrap_or("")
                        );
                    }
                }
            }
            // print summary of warnings to screen
            if warn_cnt > 0 {
                let _ = writeln!(self.msgout, "\n\nfiles with warnings:");
                let _ = writeln!(self.msgout, "------------------");
                for file_no in 0..self.file_cnt {
                    if self.err_tp[file_no] == EL_WARNING {
                        let _ = writeln!(
                            self.msgout,
                            "{} ({})",
                            self.filelist[file_no],
                            self.err_list[file_no].as_deref().unwrap_or("")
                        );
                    }
                }
            }
        }

        // show statistics
        let _ = writeln!(
            self.msgout,
            "\n\n-> {} file(s) processed, {} error(s), {} warning(s)",
            self.file_cnt, error_cnt, warn_cnt
        );
        if (self.file_cnt > error_cnt) && (self.verbosity != 0) && (self.action == A_COMPRESS) {
            acc_mp3size /= 1024.0;
            acc_pmpsize /= 1024.0;
            let total = end.duration_since(begin).as_secs_f64();
            let kbps = if total > 0.0 {
                (acc_mp3size / total) as i64
            } else {
                acc_mp3size as i64
            };
            let cr = if acc_mp3size > 0.0 {
                100.0 * acc_pmpsize / acc_mp3size
            } else {
                0.0
            };

            let _ = writeln!(self.msgout, " -------------------------------- ");
            if total >= 0.0 {
                let _ = writeln!(self.msgout, " total time        : {:8.2} sec", total);
                let _ = writeln!(self.msgout, " avrg. kbyte per s : {:8} kbps", kbps);
            } else {
                let _ = writeln!(self.msgout, " total time        : {:8} sec", "N/A");
                let _ = writeln!(self.msgout, " avrg. kbyte per s : {:8} kbps", "N/A");
            }
            let _ = writeln!(self.msgout, " avrg. comp. ratio : {:8.2} %", cr);
            let _ = writeln!(self.msgout, " -------------------------------- ");
        }

        // pause before exit
        if self.wait_exit && !self.msgout_is_stderr {
            let _ = writeln!(self.msgout, "\n\n< press ENTER >\n");
            let mut buf = String::new();
            let _ = io::stdin().read_line(&mut buf);
        }

        0
    }

    // -----------------------------------------------------------------------
    // reads in command line arguments (initialize_options)
    // -----------------------------------------------------------------------

    pub fn initialize_options(&mut self, args: &[String]) {
        // read in arguments (args[0] is the program name)
        for arg in args.iter().skip(1) {
            if arg == "-p" {
                self.err_tol = 2;
            } else if arg == "-ver" {
                if self.verify_lv < 1 {
                    self.verify_lv = 1;
                }
            } else if let Some(tmp_val) = parse_verbosity(arg) {
                // "-v%i"
                self.verbosity = tmp_val.clamp(0, 2);
            } else if arg == "-vp" {
                self.verbosity = -1;
            } else if arg == "-np" {
                self.wait_exit = false;
            } else if arg == "-o" {
                self.overwrite = true;
            } else if arg == "-" {
                // switch standard message out stream
                self.msgout_is_stderr = true;
                self.msgout = Box::new(io::stderr());
                // use "-" as placeholder for stdin
                self.filelist.push("-".to_string());
            } else {
                // if argument is not switch, it's a filename
                self.filelist.push(arg.clone());
            }
        }

        // count number of files (or filenames) in filelist
        self.file_cnt = self.filelist.len();

        // alloc arrays for error messages and types storage
        self.err_list = vec![None; self.file_cnt];
        self.err_tp = vec![0; self.file_cnt];
    }

    // -----------------------------------------------------------------------
    // shows help in case of wrong input (show_help)
    // -----------------------------------------------------------------------

    pub fn show_help(&mut self) {
        let _ = writeln!(self.msgout);
        let _ = writeln!(self.msgout, "Website: {}", WEBSITE);
        let _ = writeln!(self.msgout, "Email  : {}", EMAIL);
        let _ = writeln!(self.msgout);
        let _ = write!(self.msgout, "Usage: {} [switches] [filename(s)]", APPNAME);
        let _ = writeln!(self.msgout);
        let _ = writeln!(self.msgout);
        let _ = writeln!(self.msgout, " [-ver]   verify files after processing");
        let _ = writeln!(self.msgout, " [-v?]    set level of verbosity (max: 2) (def: 0)");
        let _ = writeln!(self.msgout, " [-np]    no pause after processing files");
        let _ = writeln!(self.msgout, " [-o]     overwrite existing files");
        let _ = writeln!(self.msgout, " [-p]     proceed on warnings");
        let _ = writeln!(self.msgout);
        let _ = writeln!(self.msgout, "Examples: \"{} -v1 -o luka.{}\"", APPNAME, PMP_EXT);
        let _ = writeln!(self.msgout, "          \"{} -p *.{}\"", APPNAME, MP3_EXT);
    }

    // -----------------------------------------------------------------------
    // UI for processing one file (process_ui)
    // -----------------------------------------------------------------------

    pub fn process_ui(&mut self) {
        let mut actionmsg: &str = "";
        let errtypemsg: &str;

        self.errorfunction = None;
        self.st.errorlevel = EL_NO_ERROR;
        self.st.mp3filesize = 0;
        self.st.pmpfilesize = 0;
        self.action = A_COMPRESS;

        // compare file name, set pipe if needed
        if (self.filelist[self.file_no] == "-") && (self.action == A_COMPRESS) {
            self.pipe_on = true;
            self.filelist[self.file_no] = "STDIN".to_string();
        } else {
            self.pipe_on = false;
        }

        if self.verbosity >= 0 {
            // standard UI
            let _ = write!(
                self.msgout,
                "\nProcessing file {} of {} \"{}\" -> ",
                self.file_no + 1,
                self.file_cnt,
                self.filelist[self.file_no]
            );

            if self.verbosity > 1 {
                // C++ uses fprintf("\n---...") without a trailing newline so the
                // first status line follows directly (no blank line)
                let _ = write!(self.msgout, "\n----------------------------------------");
            }

            // check input file and determine filetype
            self.execute(ErrorFunction::CheckFile);

            // get specific action message
            if self.st.filetype == F_UNK {
                actionmsg = "unknown filetype";
            } else {
                match self.action {
                    A_COMPRESS => {
                        actionmsg = if self.st.filetype == F_MP3 {
                            "Compressing"
                        } else {
                            "Decompressing"
                        };
                    }
                    _ => actionmsg = "",
                }
            }

            if self.verbosity < 2 {
                let _ = write!(self.msgout, "{} -> ", actionmsg);
            }
        } else {
            // progress bar UI
            // update progress message
            let _ = write!(
                self.msgout,
                "Processing file {:2} of {:2} ",
                self.file_no + 1,
                self.file_cnt
            );
            progress_bar(&mut self.msgout, self.file_no as i32, self.file_cnt as i32);
            let _ = write!(self.msgout, "\r");
            self.execute(ErrorFunction::CheckFile);
        }
        let _ = self.msgout.flush();

        // main function routine
        let begin = Instant::now();

        // streams are initiated, start processing file
        self.process_file();

        // close iostreams
        self.str_in = None;
        self.str_out = None;
        self.str_str = None;

        // delete if broken or if output not needed
        if (!self.pipe_on) && ((self.st.errorlevel >= self.err_tol) || (self.action != A_COMPRESS))
        {
            if (self.st.filetype == F_MP3) && file_exists(&self.pmpfilename) {
                let _ = std::fs::remove_file(&self.pmpfilename);
            } else if (self.st.filetype == F_PMP) && file_exists(&self.mp3filename) {
                let _ = std::fs::remove_file(&self.mp3filename);
            }
        }

        let end = Instant::now();

        // speed and compression ratio calculation
        let total = end.duration_since(begin).as_millis() as i64;
        let bpms = if total > 0 {
            self.st.mp3filesize / total as i32
        } else {
            self.st.mp3filesize
        };
        let cr = if self.st.mp3filesize > 0 {
            100.0 * self.st.pmpfilesize as f64 / self.st.mp3filesize as f64
        } else {
            0.0
        };

        if self.verbosity >= 0 {
            // standard UI
            if self.verbosity > 1 {
                // C++ fprintf("\n---...") has no trailing newline: the
                // "-> Compressing OK" line follows directly (no blank line)
                let _ = write!(self.msgout, "\n----------------------------------------");
            }

            // display success/failure message
            match self.verbosity {
                0 => {
                    if self.st.errorlevel < self.err_tol {
                        if self.action == A_COMPRESS {
                            let _ = write!(self.msgout, "{:.2}%", cr);
                        } else {
                            let _ = write!(self.msgout, "DONE");
                        }
                    } else {
                        let _ = write!(self.msgout, "ERROR");
                    }
                    if self.st.errorlevel > 0 {
                        let _ = writeln!(self.msgout);
                    }
                }
                1 => {
                    let _ = writeln!(
                        self.msgout,
                        "{}",
                        if self.st.errorlevel < self.err_tol {
                            "DONE"
                        } else {
                            "ERROR"
                        }
                    );
                }
                2 => {
                    if self.st.errorlevel < self.err_tol {
                        let _ = writeln!(self.msgout, "\n-> {} OK", actionmsg);
                    } else {
                        let _ = writeln!(self.msgout, "\n-> {} ERROR", actionmsg);
                    }
                }
                _ => {}
            }

            // set type of error message
            match self.st.errorlevel {
                0 => errtypemsg = "none",
                1 => {
                    errtypemsg = if self.err_tol > 1 {
                        "warning (ignored)"
                    } else {
                        "warning (skipped file)"
                    }
                }
                _ => errtypemsg = "fatal error",
            }

            // error/warning message
            if self.st.errorlevel > 0 {
                let status = self
                    .errorfunction
                    .map_or("unknown action", |f| f.status());
                let _ = writeln!(self.msgout, " {} -> {}:", status, errtypemsg);
                let _ = writeln!(self.msgout, " {}", self.st.errormessage);
            }
            if (self.verbosity > 0)
                && (self.st.errorlevel < self.err_tol)
                && (self.action == A_COMPRESS)
            {
                if total >= 0 {
                    let _ = writeln!(self.msgout, " time taken  : {:7} msec", total);
                    let _ = writeln!(self.msgout, " byte per ms : {:7} byte", bpms);
                } else {
                    let _ = writeln!(self.msgout, " time taken  : {:7} msec", "N/A");
                    let _ = writeln!(self.msgout, " byte per ms : {:7} byte", "N/A");
                }
                let _ = writeln!(self.msgout, " comp. ratio : {:7.2} %", cr);
            }
            if (self.verbosity > 1) && (self.action == A_COMPRESS) {
                let _ = writeln!(self.msgout);
            }
        } else {
            // progress bar UI
            // if this is the last file, update progress bar one last time
            if self.file_no + 1 == self.file_cnt {
                // update progress message
                let _ = write!(
                    self.msgout,
                    "Processed {:2} of {:2} files ",
                    self.file_no + 1,
                    self.file_cnt
                );
                progress_bar(&mut self.msgout, 1, 1);
                let _ = write!(self.msgout, "\r");
            }
        }
    }

    // -----------------------------------------------------------------------
    // processes one file (process_file)
    // -----------------------------------------------------------------------

    fn process_file(&mut self) {
        if (self.st.filetype == F_MP3) && (self.action == A_COMPRESS) {
            self.execute(ErrorFunction::ReadMp3);
            self.execute(ErrorFunction::AnalyzeFrames);
            self.execute(ErrorFunction::CompressMp3);
            if self.verify_lv > 0 {
                // verification
                self.execute(ErrorFunction::ResetBuffers);
                self.execute(ErrorFunction::SwapStreams);
                self.execute(ErrorFunction::UncompressPmp);
                self.execute(ErrorFunction::WriteMp3);
                self.execute(ErrorFunction::CompareOutput);
            }
        } else if (self.st.filetype == F_PMP) && (self.action == A_COMPRESS) {
            self.execute(ErrorFunction::UncompressPmp);
            self.execute(ErrorFunction::WriteMp3);
            if self.verify_lv > 0 {
                // verify
                self.execute(ErrorFunction::ResetBuffers);
                self.execute(ErrorFunction::SwapStreams);
                self.execute(ErrorFunction::ReadMp3);
                self.execute(ErrorFunction::AnalyzeFrames);
                self.execute(ErrorFunction::CompressMp3);
                self.execute(ErrorFunction::CompareOutput);
            }
        }
        // reset buffers
        app::reset_buffers(&mut self.st);
    }

    // -----------------------------------------------------------------------
    // main-function execution routine (execute)
    // -----------------------------------------------------------------------

    fn execute(&mut self, func: ErrorFunction) {
        if self.st.errorlevel < self.err_tol {
            // write statusmessage
            if self.verbosity == 2 {
                let s = func.status();
                let _ = write!(self.msgout, "\n{} ", s);
                for _ in s.len()..=30 {
                    let _ = write!(self.msgout, " ");
                }
            }

            // call function
            let begin = Instant::now();
            let success = func.run(self);
            let end = Instant::now();

            if (self.st.errorlevel > 0) && self.errorfunction.is_none() {
                self.errorfunction = Some(func);
            }

            // write time or failure notice
            if success {
                let total = end.duration_since(begin).as_millis() as i64;
                if self.verbosity == 2 {
                    let _ = write!(self.msgout, "{:6}ms", if total >= 0 { total } else { -1 });
                }
            } else {
                self.errorfunction = Some(func);
                if self.verbosity == 2 {
                    let _ = write!(self.msgout, "{:8}", "ERROR");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // check file and determine filetype (check_file)
    // -----------------------------------------------------------------------

    fn check_file(&mut self) -> bool {
        let mut fileid = [0u8; 2];
        let filename = self.filelist[self.file_no].clone();

        // open input stream, check for errors
        if self.pipe_on {
            self.str_in = Some(IoStream::new(&IoSource::Stdio, TYPE_STREAM, 0, MODE_READ));
        } else {
            self.str_in = Some(IoStream::new(&IoSource::File(&filename), TYPE_FILE, 0, MODE_READ));
        }
        if self.str_in.as_ref().unwrap().chkerr() {
            self.st.errormessage = FRD_ERRMSG.to_string();
            self.st.errorlevel = EL_FATAL;
            return false;
        }

        // free memory from filenames if needed
        self.mp3filename.clear();
        self.pmpfilename.clear();

        // immediately return error if 2 bytes can't be read
        if self.str_in.as_mut().unwrap().read(&mut fileid, 1, 2) != 2 {
            self.st.filetype = F_UNK;
            self.st.errormessage = "file doesn't contain enough data".to_string();
            self.st.errorlevel = EL_FATAL;
            return false;
        }

        // rewind (need to start from the beginning)
        if self.str_in.as_mut().unwrap().rewind() != 0 {
            self.st.errormessage = FRD_ERRMSG.to_string();
            self.st.errorlevel = EL_FATAL;
            return false;
        }

        // check file id, determine filetype
        if (fileid[0] == PMP_MAGIC[0]) && (fileid[1] == PMP_MAGIC[1]) {
            // PMP marker -> file is PMP
            self.st.filetype = F_PMP;
            // create filenames
            if !self.pipe_on {
                self.pmpfilename = filename.clone();
                self.mp3filename = if self.overwrite {
                    create_filename(&filename, Some(MP3_EXT))
                } else {
                    unique_filename(&filename, MP3_EXT)
                };
                let out_name = self.mp3filename.clone();
                self.open_output(&out_name);
            } else {
                self.mp3filename = create_filename("STDOUT", None);
                self.pmpfilename = create_filename("STDIN", None);
                self.open_output(&self.mp3filename.clone());
            }
            if self.str_out.as_ref().unwrap().chkerr() {
                self.st.errormessage = FWR_ERRMSG.to_string();
                self.st.errorlevel = EL_FATAL;
                return false;
            }
        } else {
            // for all other cases we assume that file might be MPEG X LAYER Y
            self.st.filetype = F_MP3;
            // create filenames
            if !self.pipe_on {
                self.mp3filename = filename.clone();
                self.pmpfilename = if self.overwrite {
                    create_filename(&filename, Some(PMP_EXT))
                } else {
                    unique_filename(&filename, PMP_EXT)
                };
                let out_name = self.pmpfilename.clone();
                self.open_output(&out_name);
            } else {
                self.mp3filename = create_filename("STDIN", None);
                self.pmpfilename = create_filename("STDOUT", None);
                self.open_output(&self.pmpfilename.clone());
            }
            if self.str_out.as_ref().unwrap().chkerr() {
                self.st.errormessage = FWR_ERRMSG.to_string();
                self.st.errorlevel = EL_FATAL;
                return false;
            }
        }

        true
    }

    /// open output stream for the current file (file or stdout when piping)
    fn open_output(&mut self, filename: &str) {
        if self.pipe_on {
            self.str_out = Some(IoStream::new(&IoSource::Stdio, TYPE_STREAM, 0, MODE_WRITE));
        } else {
            self.str_out = Some(IoStream::new(&IoSource::File(filename), TYPE_FILE, 0, MODE_WRITE));
        }
    }

    // -----------------------------------------------------------------------
    // swap streams / init verification (swap_streams)
    // -----------------------------------------------------------------------

    fn swap_streams(&mut self) -> bool {
        // store input stream
        self.str_str = self.str_in.take();
        if let Some(s) = self.str_str.as_mut() {
            s.rewind();
        }

        // replace input stream by output stream / switch mode for reading
        self.str_in = self.str_out.take();
        if let Some(s) = self.str_in.as_mut() {
            s.switch_mode();
        }

        // open new stream for output / check for errors
        self.str_out = Some(IoStream::new(&IoSource::Memory(&[]), TYPE_MEMORY, 0, MODE_WRITE));
        if self.str_out.as_ref().unwrap().chkerr() {
            self.st.errormessage = "error opening comparison stream".to_string();
            self.st.errorlevel = EL_FATAL;
            return false;
        }

        true
    }

    // -----------------------------------------------------------------------
    // comparison between input & output (compare_output)
    // -----------------------------------------------------------------------

    fn compare_output(&mut self) -> bool {
        let bsize = 1024usize;

        // switch output stream mode / check for stream errors
        self.str_out.as_mut().unwrap().switch_mode();
        let mut err = None;
        if self.str_out.as_ref().unwrap().chkerr() {
            err = Some("error in comparison stream");
        } else if self.str_in.as_ref().unwrap().chkerr() {
            err = Some("error in output stream");
        } else if self.str_str.as_ref().unwrap().chkerr() {
            err = Some("error in input stream");
        }
        if let Some(e) = err {
            self.st.errormessage = e.to_string();
            self.st.errorlevel = EL_FATAL;
            return false;
        }

        // compare sizes
        let dsize = self.str_str.as_ref().unwrap().getsize();
        if self.str_out.as_ref().unwrap().getsize() != dsize {
            self.st.errormessage = "file sizes do not match".to_string();
            self.st.errorlevel = EL_FATAL;
            return false;
        }

        // compare files byte by byte
        let mut buff_ori = vec![0u8; bsize];
        let mut buff_cmp = vec![0u8; bsize];
        for i in 0..dsize {
            let b = i % bsize;
            if b == 0 {
                self.str_str
                    .as_mut()
                    .unwrap()
                    .read(&mut buff_ori, 1, bsize as i32);
                self.str_out
                    .as_mut()
                    .unwrap()
                    .read(&mut buff_cmp, 1, bsize as i32);
            }
            if buff_ori[b] != buff_cmp[b] {
                self.st.errormessage = format!("difference found at 0x{:X}", i);
                self.st.errorlevel = EL_FATAL;
                return false;
            }
        }

        true
    }
}

// ---------------------------------------------------------------------------
// helper functions
// ---------------------------------------------------------------------------

/// mimics sscanf("-v%i"): parses an integer (with optional sign) after "-v".
/// Returns None when the pattern doesn't match.
fn parse_verbosity(s: &str) -> Option<i32> {
    let rest = s.strip_prefix("-v")?;
    let mut chars = rest.char_indices();
    let (neg, skip) = match chars.next() {
        Some((_, '+')) => (false, 1),
        Some((_, '-')) => (true, 1),
        Some((_, c)) if c.is_ascii_digit() => (false, 0),
        _ => return None,
    };
    let digits: String = rest[skip..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    let v: i64 = digits.parse().ok()?;
    let v = if neg { -v } else { v };
    // C `%i` returns an int; clamp to i32 range
    Some(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
}

/// generates a progress bar (BARLEN wide); filled part uses byte 0xFE like
/// the _WIN32 build of the original.
fn progress_bar(out: &mut dyn Write, current: i32, last: i32) {
    let barpos = ((current * BARLEN) + (last / 2)) / last;
    let _ = write!(out, "[");
    for _ in 0..barpos {
        let _ = out.write(&[0xFE]);
    }
    for _ in barpos..BARLEN {
        let _ = write!(out, " ");
    }
    let _ = write!(out, "]");
}

/// checks if a file exists (file_exists)
pub fn file_exists(filename: &str) -> bool {
    std::path::Path::new(filename).exists()
}

/// creates a filename from base & extension (create_filename)
pub fn create_filename(base: &str, extension: Option<&str>) -> String {
    let mut filename = base.to_string();
    set_extension(&mut filename, extension);
    filename
}

/// changes extension of filename (set_extension)
fn set_extension(filename: &mut String, extension: Option<&str>) {
    // find position of last '.' (or end of string)
    let dot = filename.rfind('.');
    match extension {
        Some(ext) => {
            // replace/extend extension
            let base_len = dot.unwrap_or(filename.len());
            filename.truncate(base_len);
            filename.push('.');
            filename.push_str(ext);
        }
        None => {
            // strip extension
            if let Some(d) = dot {
                filename.truncate(d);
            }
        }
    }
}

/// adds underscore before the extension (add_underscore)
fn add_underscore(filename: &mut String) {
    if let Some(d) = filename.rfind('.') {
        filename.insert(d, '_');
    } else {
        filename.push('_');
    }
}

/// creates a unique filename using underscores (unique_filename)
pub fn unique_filename(base: &str, extension: &str) -> String {
    let mut filename = create_filename(base, Some(extension));
    while file_exists(&filename) {
        add_underscore(&mut filename);
    }
    filename
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn parse_verbosity_matches() {
        assert_eq!(parse_verbosity("-v0"), Some(0));
        assert_eq!(parse_verbosity("-v1"), Some(1));
        assert_eq!(parse_verbosity("-v2"), Some(2));
        assert_eq!(parse_verbosity("-v5"), Some(5));
        assert_eq!(parse_verbosity("-v-1"), Some(-1));
        assert_eq!(parse_verbosity("-v+3"), Some(3));
        // non-matching patterns
        assert_eq!(parse_verbosity("-ver"), None);
        assert_eq!(parse_verbosity("-vp"), None);
        assert_eq!(parse_verbosity("-v"), None);
        assert_eq!(parse_verbosity("file.mp3"), None);
    }

    #[test]
    fn filename_extension_handling() {
        // replace extension
        let mut s = "song.mp3".to_string();
        set_extension(&mut s, Some("pmp"));
        assert_eq!(s, "song.pmp");
        // no dot -> append extension
        let mut s = "song".to_string();
        set_extension(&mut s, Some("pmp"));
        assert_eq!(s, "song.pmp");
        // strip extension
        let mut s = "song.mp3".to_string();
        set_extension(&mut s, None);
        assert_eq!(s, "song");
        // last dot only
        let mut s = "a.b.c.mp3".to_string();
        set_extension(&mut s, Some("pmp"));
        assert_eq!(s, "a.b.c.pmp");
    }

    #[test]
    fn filename_underscore_handling() {
        let mut s = "song.pmp".to_string();
        add_underscore(&mut s);
        assert_eq!(s, "song_.pmp");
        let mut s = "song".to_string();
        add_underscore(&mut s);
        assert_eq!(s, "song_");
        let mut s = "song_.pmp".to_string();
        add_underscore(&mut s);
        assert_eq!(s, "song__.pmp");
    }

    #[test]
    fn create_filename_works() {
        assert_eq!(create_filename("song.mp3", Some("pmp")), "song.pmp");
        assert_eq!(create_filename("song", Some("pmp")), "song.pmp");
        assert_eq!(create_filename("song.mp3", None), "song");
    }

    #[test]
    fn options_parsing() {
        let args: Vec<String> = vec![
            "packmp3".into(),
            "-p".into(),
            "-ver".into(),
            "-v2".into(),
            "-np".into(),
            "-o".into(),
            "a.mp3".into(),
        ];
        let mut cli = Cli::new();
        cli.initialize_options(&args);
        assert_eq!(cli.err_tol, 2);
        assert_eq!(cli.verify_lv, 1);
        assert_eq!(cli.verbosity, 2);
        assert!(!cli.wait_exit);
        assert!(cli.overwrite);
        assert_eq!(cli.filelist, vec!["a.mp3"]);
        assert_eq!(cli.file_cnt, 1);
    }

    #[test]
    fn options_defaults() {
        let args: Vec<String> = vec!["packmp3".into(), "song.mp3".into()];
        let mut cli = Cli::new();
        cli.initialize_options(&args);
        assert_eq!(cli.err_tol, 1);
        assert_eq!(cli.verify_lv, 0);
        assert_eq!(cli.verbosity, -1); // default: progress bar UI
        assert!(cli.wait_exit);
        assert!(!cli.overwrite);
        assert_eq!(cli.filelist, vec!["song.mp3"]);
    }

    #[test]
    fn options_dash_uses_stdin_and_stderr() {
        let args: Vec<String> = vec!["packmp3".into(), "-".into()];
        let mut cli = Cli::new();
        cli.initialize_options(&args);
        assert!(cli.msgout_is_stderr);
        assert_eq!(cli.filelist, vec!["-"]);
    }

    #[test]
    fn status_messages() {
        assert_eq!(ErrorFunction::CheckFile.status(), "Determining filetype");
        assert_eq!(ErrorFunction::ReadMp3.status(), "Reading MP3");
        assert_eq!(ErrorFunction::CompressMp3.status(), "Compressing to PMP");
        assert_eq!(ErrorFunction::CompareOutput.status(), "Verifying output stream");
    }

    #[test]
    fn progress_bar_shape() {
        let mut buf: Vec<u8> = Vec::new();
        progress_bar(&mut buf, 1, 1);
        // barpos = (1*36 + 0)/1 = 36
        assert_eq!(buf.len(), 38); // '[' + 36 + ']'
        assert_eq!(buf[0], b'[');
        assert_eq!(buf[37], b']');
        assert!(buf[1..37].iter().all(|&b| b == 0xFE));
        let mut buf: Vec<u8> = Vec::new();
        progress_bar(&mut buf, 0, 4);
        // barpos = (0*36 + 2)/4 = 0
        assert_eq!(buf, {
            let mut v = vec![b'['];
            v.resize(37, b' ');
            v.push(b']');
            v
        });
    }

    #[test]
    fn run_main_help_when_no_files() {
        let out = Rc::new(RefCell::new(Vec::<u8>::new()));
        let mut cli = Cli::new();
        cli.msgout = Box::new(TestWriter(out.clone()));
        let rc = cli.run_main(&["packmp3".to_string()]);
        assert_eq!(rc, -1);
        let out = String::from_utf8(out.borrow().clone()).unwrap();
        assert!(out.contains("Usage: packMP3 [switches] [filename(s)]"));
        assert!(out.contains("Website: http://packjpg.encode.ru/"));
    }

    /// test writer that appends into a shared byte buffer
    struct TestWriter(Rc<RefCell<Vec<u8>>>);

    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
