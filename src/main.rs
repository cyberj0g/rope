mod config;
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
    let config = Config::load()?;
    let provider = OpenAiProvider::new(config.base_url.clone(), config.api_key.clone());
    let tools = tool::discover(&config).await?;
    let (command_tx, event_rx) = runtime::spawn(config.clone(), startup, provider, tools).await?;

    let price_per_token = config.price_per_token;
    let summary = ui::run(config, command_tx, event_rx, request).await?;
    println!("tokens used: {}", summary.total_tokens);
    println!(
        "estimated cost: ${:.6}",
        summary.total_tokens as f64 * price_per_token
    );
    println!(
        "resume with: rope --session '{}'",
        summary.name.replace('\'', "'\"'\"'")
    );
    Ok(())
}
