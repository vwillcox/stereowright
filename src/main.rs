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
        None => gui::run(args.images, lockdown),
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
