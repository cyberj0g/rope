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
    let config = Config::load(args)?;
    let provider = OpenAiProvider::new(config.base_url.clone(), config.api_key.clone());
    let tools = tool::discover(&config).await?;
    let (command_tx, event_rx) = runtime::spawn(config.clone(), startup, provider, tools).await?;

    ui::run(config, command_tx, event_rx).await
}
