#![warn(clippy::pedantic)]
use std::sync::atomic::AtomicBool;

use clap::Parser;

use crate::primitives::{Args, Bapple};

type Res<T> = std::result::Result<T, Box<dyn std::error::Error>>;

mod backup_counter;
mod messages;
mod primitives;

static STOP: AtomicBool = AtomicBool::new(false);

fn main() -> Res<()> {
    ctrlc::set_handler(ctrl_c)?;
    let args = Args::parse();

    let mut bapple = Bapple::new(args.file)?;

    if args.frames_per_second != 0.0 {
        bapple.set_frametime(1_000_000.0 / args.frames_per_second);
    }

    loop {
        bapple.play()?;
        if !args.r#loop {
            break;
        }
    }
    Ok(())
}

fn ctrl_c() {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}
