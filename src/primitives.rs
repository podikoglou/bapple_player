use std::{
    fs::File,
    io::{self, Cursor, Read, Write, stdout},
    path::PathBuf,
    process::exit,
    sync::{Arc, atomic::Ordering},
    thread::{sleep, spawn},
    time::{Duration, Instant},
};

use clap::{Parser, crate_version};
use rodio::{Decoder, OutputStreamBuilder, Sink, Source};
use ron::de::from_bytes;
use serde::Deserialize;
use tar::{Archive, Entry};
use zstd::decode_all;

use crate::{
    Res, STOP,
    backup_counter::{SYNC_COUNTER, outside_counter},
    messages::FRAMETIME_ZERO,
};

pub struct Bapple {
    compressed_frames: Vec<Vec<u8>>,
    audio: Arc<[u8]>, // May be empty
    has_audio: bool,
    frametime: Duration,
    counter: usize,
    length: usize,
}

impl Drop for Bapple {
    fn drop(&mut self) {
        let _ = show_cursor(&mut stdout().lock());
    }
}

impl Bapple {
    pub fn new(path: PathBuf) -> Res<Self> {
        println!("Processing frames...");

        let mut audio = Vec::new();
        let mut has_audio = false;
        let mut frametime = 0;

        let compressed_frames = Archive::new(File::open(path)?)
            .entries()?
            .filter_map(|e| {
                Self::process_frames(
                    e,
                    &mut has_audio,
                    &mut audio,
                    &mut frametime,
                )
            })
            .collect::<Vec<_>>();

        let length = compressed_frames.len();

        Ok(Self {
            compressed_frames,
            audio: audio.into(),
            has_audio,
            frametime: Duration::from_micros(frametime),
            counter: 0,
            length,
        })
    }

    pub fn play(&mut self) -> Res<()> {
        if self.frametime.is_zero() {
            eprintln!("{FRAMETIME_ZERO}");
            exit(1);
        }

        #[cfg(target_os = "linux")]
        if self.has_audio {
            Self::check_alsa_config();
        }

        let mut sink = None;
        let mut total = None;

        // Don't drop prematurely, or else the audio won't play.
        let output_stream = OutputStreamBuilder::open_default_stream()?;

        if self.has_audio {
            let decoder = Decoder::new_mp3(Cursor::new(self.audio.clone()))?;
            let inner_total = decoder.total_duration().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Unable to determine audio duration",
                )
            })?;
            total = Some(inner_total);
            let source = decoder.track_position();

            let inner_sink = Sink::connect_new(output_stream.mixer());
            inner_sink.append(source);
            inner_sink.play();
            sink = Some(inner_sink);
        } else {
            let frametime = self.frametime;
            let length = self.length;
            spawn(move || outside_counter(frametime, length));
        }

        let mut lock = stdout().lock();

        #[cfg(windows)]
        enable_virtual_terminal_processing();

        clear(&mut lock)?;
        hide_cursor(&mut lock)?;

        while self.counter < self.length {
            let task_time = Instant::now();
            let decompressed_frame =
                decode_all(&*self.compressed_frames[self.counter])?;

            return_home(&mut lock)?;
            lock.write_all(&decompressed_frame)?;
            lock.flush()?;

            if !self.counter.is_multiple_of(15) {
                self.counter += 1;
            } else if self.has_audio {
                // Same condition, safe unwrap.
                self.counter =
                    self.get_pos(sink.as_ref().unwrap(), total.unwrap());
                if STOP.load(Ordering::Relaxed) {
                    return Err("Stopped".into());
                }
            } else {
                self.backup_resync();
                if STOP.load(Ordering::Relaxed) {
                    return Err("Stopped".into());
                }
            }

            if let Some(remaining) =
                self.frametime.checked_sub(task_time.elapsed())
            {
                sleep(remaining);
            }
        }

        show_cursor(&mut lock)?;
        self.counter = 0;
        SYNC_COUNTER.store(0, Ordering::Relaxed);
        Ok(())
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn get_pos(&self, sink: &Sink, total: Duration) -> usize {
        (sink.get_pos().div_duration_f64(total) * self.length as f64).round()
            as usize
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn set_frametime(&mut self, frametime: f64) {
        self.frametime = Duration::from_micros(frametime as u64);
    }

    pub fn backup_resync(&mut self) {
        self.counter = SYNC_COUNTER.load(Ordering::Relaxed);
    }

    #[cfg(target_os = "linux")]
    fn check_alsa_config() {
        use crate::messages::ALSA_WARNING;
        use std::{path::Path, thread::sleep, time::Duration};

        if !Path::new("/etc/alsa/conf.d").exists() {
            eprintln!("{ALSA_WARNING}");
            sleep(Duration::from_secs(5));
        }
    }

    fn process_frames(
        entry: Result<Entry<'_, File>, io::Error>,
        has_audio: &mut bool,
        audio: &mut Vec<u8>,
        outer_frametime: &mut u64,
    ) -> Option<Vec<u8>> {
        let mut entry = entry.ok()?;
        let file_stem = entry.header().path().ok()?.file_stem()?.to_os_string();

        let mut content = Vec::new();
        entry.read_to_end(&mut content).ok()?;

        if file_stem == *"audio" {
            *has_audio = true;
            *audio = content;

            return None;
        } else if file_stem == *"metadata" {
            let Metadata { frametime, fps } =
                from_bytes(&content).unwrap_or_default();
            if frametime != 0 {
                *outer_frametime = frametime;
            } else if fps != 0 {
                // DEPRECATED
                *outer_frametime = 1_000_000 / fps;
            }
            // No further processing, since this can be
            // overriden by the FPS arg
            return None;
        }

        Some(content)
    }
}

/// Asciix on cocaine
#[derive(Parser, Debug)]
#[command(version(crate_version!()))]
pub struct Args {
    /// Path to a .bapple file.
    pub file: PathBuf,
    /// Should be self-explanatory.
    #[arg(default_value = "0", value_parser = validate_fps)]
    pub frames_per_second: f64,
    /// Enables looping
    #[arg(short, long)]
    pub r#loop: bool,
}

fn validate_fps(s: &str) -> std::result::Result<f64, String> {
    let fps: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if fps != 0.0 /*Value for autodetect*/ && fps < 0.01 {
        return Err("FPS value is too small.".to_string());
    }
    Ok(fps)
}

#[derive(Deserialize, Default)]
pub struct Metadata {
    frametime: u64,
    /// DEPRECATED
    fps: u64,
}

#[cfg(windows)]
fn enable_virtual_terminal_processing() {
    use winapi::um::consoleapi::GetConsoleMode;
    use winapi::um::consoleapi::SetConsoleMode;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::winbase::STD_OUTPUT_HANDLE;
    use winapi::um::wincon::ENABLE_VIRTUAL_TERMINAL_PROCESSING;

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle != INVALID_HANDLE_VALUE {
            let mut mode = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                if SetConsoleMode(
                    handle,
                    mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                ) == 0 {
                    eprintln!("Warning: Failed to enable virtual terminal processing");
                }
            }
        }
    }
}

macro_rules! write_fn {
    ($fn_name:ident, $val:expr) => {
        #[inline]
        fn $fn_name<W: std::io::Write>(w: &mut W) -> std::io::Result<()> {
            w.write_all($val)
        }
    };
}

write_fn!(clear, b"\r\x1b[2J\x1b[H");
write_fn!(show_cursor, b"\x1b[?25h");
write_fn!(hide_cursor, b"\x1b[?25l");
write_fn!(return_home, b"\x1b[H");
