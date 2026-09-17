use anyhow::Result;
use clap::Parser;
use rope::{
    config::{Args, Config},
    core::Core,
    onboarding,
    provider::openai::OpenAiProvider,
    server::Server,
    session, ui,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !Config::global_exists()? {
        if args.headless {
            anyhow::bail!(
                "no Rope configuration; run rope interactively to configure a provider first"
            );
        }
        onboarding::run().await?;
    }
    let config = Config::load()?;
    for notice in config.notices() {
        eprintln!("warning: {notice}");
    }
    let core = Core::new(
        config.clone(),
        std::env::current_dir()?,
        session::sessions_root()?,
        Arc::new(OpenAiProvider::from_config(&config)),
    )
    .await?;
    let result = run(&args, config, core.clone()).await;
    let stopped = core.shutdown().await;
    result?;
    stopped?;
    Ok(())
}

async fn run(args: &Args, config: Config, core: Core) -> Result<()> {
    let mut server = if args.headless || args.listen.is_some() {
        let (token, path) = rope::server::load_token(args.token_file.clone())?;
        let address = args
            .listen
            .unwrap_or_else(|| "127.0.0.1:8787".parse().unwrap());
        let server = Server::start(core.clone(), address, token, args.allow_origin.clone()).await?;
        eprintln!("Rope listening on http://{}", server.address);
        if let Some(path) = path {
            eprintln!("server token: {}", path.display());
        }
        Some(server)
    } else {
        None
    };
    let mut summary = None;
    let result: Result<()> = async {
        if args.headless {
            if args.session.is_some() || args.request.is_some() {
                let id = core.open(args.session.clone()).await?;
                if let Some(content) = &args.request {
                    core.command(&id, rope::protocol::Action::SendMessage { content: content.clone(), attachments: Vec::new() }).await?;
                }
            }
        } else {
            let id = core.open(args.session.clone()).await?;
            tokio::select! {
                result = ui::run(config, core.clone(), id, args.request.clone()) => { summary = Some(result?); }
                result = shutdown_signal() => { result?; return Ok(()); }
                result = server_failure(&mut server) => { return result; }
            }
            if server.is_some() { eprintln!("TUI detached; server is still running. Press Ctrl-C to stop it."); }
        }
        if server.is_some() {
            tokio::select! {
                result = shutdown_signal() => result?,
                result = server_failure(&mut server) => result?,
            }
        }
        Ok(())
    }.await;
    if let Some(server) = &mut server {
        server.stop();
        if !server.task.is_finished() {
            (&mut server.task).await??;
        }
    }
    if let Some(summary) = summary {
        println!("tokens used: {}", summary.total_tokens);
        if let Some(cost) = summary.total_cost {
            println!("estimated cost: ${cost:.6}");
        }
        println!(
            "resume with: rope --session '{}'",
            summary.name.replace('\'', "'\"'\"'")
        );
    }
    result
}

async fn server_failure(server: &mut Option<Server>) -> Result<()> {
    if let Some(server) = server {
        (&mut server.task).await??;
        anyhow::bail!("server stopped unexpectedly");
    }
    std::future::pending().await
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
