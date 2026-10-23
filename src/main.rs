//! Entrypoint for `Node.Application.Service.Wrapper`:
//! 1. `--send-ctrl-c <pid>`: runs the fast Ctrl+C console helper and exits immediately (no host or runtime built).
//! 2. `list | status | start | stop | restart | logs | reload`: runs the CLI against the Control API and exits.
//! 3. `(no subcommand)`: runs the multi-application Windows Service / console host.

use clap::Parser;
use node_application_service_wrapper::cli::{run_cli_command, CliArgs};
use node_application_service_wrapper::constants::exit_codes;
use node_application_service_wrapper::platform::execute_send_ctrl_c_helper;
use node_application_service_wrapper::service::{resolve_base_directory, run_service_or_console};
use std::env;
use std::process;

fn main() {
    // Fast path for `--send-ctrl-c <pid>` before building any runtime or parsing complex flags
    let raw_args: Vec<String> = env::args().collect();
    if raw_args.len() >= 2 && raw_args[1] == "--send-ctrl-c" {
        if raw_args.len() == 3 {
            if let Ok(pid) = raw_args[2].parse::<u32>() {
                process::exit(execute_send_ctrl_c_helper(pid));
            }
        }
        process::exit(exit_codes::HELPER_INVALID_ARGS);
    }

    let cli = CliArgs::parse();

    if let Some(pid) = cli.send_ctrl_c {
        process::exit(execute_send_ctrl_c_helper(pid));
    }

    let base_dir = resolve_base_directory();

    if let Some(command) = cli.command {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("Failed to initialize CLI runtime: {e}");
                process::exit(1);
            }
        };
        let code = rt.block_on(run_cli_command(command, cli.url.as_deref(), &base_dir));
        process::exit(code);
    }

    let exit_code = run_service_or_console(base_dir);
    process::exit(exit_code);
}
