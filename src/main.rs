//! Stereowright: photos → VR-ready 3D, using a local AI depth model. Fully offline.

mod cli;
mod depth;
mod export;
mod gui;
mod models;
mod pipeline;
mod postprocess;
mod qr;
mod quest;
mod sandbox;
mod settings;
mod stereo;
mod temporal;
mod upscale;
mod video;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    // First, before any thread exists: make it impossible for this process to use the network.
    let lockdown = sandbox::lock_down();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("stereowright=info,warn"))
        .format_timestamp(None)
        .init();

    let args = cli::Cli::parse();
    let result = match args.command {
        None => {
            #[cfg(windows)]
            hide_own_console();
            gui::run(args.images, lockdown)
        }
        Some(cli::Command::Convert(c)) => cli::convert(c, &lockdown),
        Some(cli::Command::Models) => cli::models(),
        Some(cli::Command::Doctor) => cli::doctor(&lockdown),
        Some(cli::Command::Settings) => cli::print_settings(),
        Some(cli::Command::Quest { action }) => cli::quest(action),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Started by double-click on Windows, the GUI would sit behind an empty console window.
/// Drop that console when it belongs to us alone (it stays when run from a terminal, so the
/// command-line tools still print there).
#[cfg(windows)]
fn hide_own_console() {
    unsafe extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }
    let mut ids = [0u32; 2];
    // SAFETY: plain kernel32 calls with a valid buffer of the given length.
    unsafe {
        if GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) == 1 {
            FreeConsole();
        }
    }
}
