mod config;
mod model_catalog;
mod onboarding;
mod project;
mod provider;
mod runtime;
mod session;
mod tool;
mod ui;

use anyhow::Result;
use clap::Parser;

use config::{Args, Config};
use provider::openai::OpenAiProvider;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let startup = args.startup();
    let request = args.request;
    if !Config::global_exists()? {
        onboarding::run().await?;
    }
    let config = Config::load()?;
    for notice in config.notices() {
        eprintln!("warning: {notice}");
    }
    let provider = OpenAiProvider::from_config(&config);
    let tools = tool::discover(&config).await?;
    let (command_tx, event_rx) = runtime::spawn(config.clone(), startup, provider, tools).await?;

    let summary = ui::run(config, command_tx, event_rx, request).await?;
    println!("tokens used: {}", summary.total_tokens);
    if let Some(cost) = summary.total_cost {
        println!("estimated cost: ${cost:.6}");
    }
    println!(
        "resume with: rope --session '{}'",
        summary.name.replace('\'', "'\"'\"'")
    );
    Ok(())
}
