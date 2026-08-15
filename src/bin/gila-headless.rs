//! Portable evaluator entry point for `gila solve`.

use clap::Parser;
use gilamonster_agent::{solve, Cli, Command};

fn main() -> anyhow::Result<()> {
    if let Some(code) = newt_core::maybe_dispatch() {
        std::process::exit(code);
    }

    let cli = Cli::parse();
    let command = cli
        .command
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("the headless binary requires `gila solve`"))?;
    let posture = cli.launch_posture_for(command);
    let Command::Solve {
        cwd,
        instruction_file,
        config,
        model,
        unsafe_host_exec,
        events,
        max_rounds,
        context_window,
        model_digest,
    } = command
    else {
        anyhow::bail!("the headless binary only supports `gila solve`");
    };
    let prepared = solve::prepare(config)?;
    posture.apply_and_freeze_with_config(Some(prepared.config()));

    let clean = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(solve::run(
            prepared,
            solve::SolveArgs {
                cwd: cwd.clone(),
                instruction_file: instruction_file.clone(),
                model: model.clone(),
                unsafe_host_exec: *unsafe_host_exec,
                events: events.clone(),
                max_rounds: *max_rounds,
                context_window: *context_window,
                model_digest: model_digest.clone(),
            },
        ))?;
    if clean {
        Ok(())
    } else {
        anyhow::bail!("Gila solve did not complete cleanly")
    }
}
