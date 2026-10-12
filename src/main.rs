use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = xolotl::cli::Cli::parse();
    if !matches!(cli.command, xolotl::cli::Command::Serve { .. }) {
        xolotl::telemetry::init_cli_logging();
    }
    if let Err(e) = xolotl::cli::run(cli).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
