use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::MoveToColumn,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor, Stylize},
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use url::Url;

use crate::{
    config::{Config, ProviderConfig, config_root},
    model_catalog::{model_config, prioritize_openai_models, supports_chat_completions},
    provider::openai::OpenAiProvider,
};

struct ProviderSummary {
    name: String,
    endpoint: String,
    models: usize,
}

struct BrowserSummary {
    executable: Option<PathBuf>,
    runtime_ready: bool,
}

struct InputMode;

impl InputMode {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for InputMode {
    fn drop(&mut self) {
        execute!(io::stdout(), DisableBracketedPaste, ResetColor).ok();
        disable_raw_mode().ok();
    }
}

pub async fn run() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "Rope needs first-time setup in an interactive terminal; create ~/.config/rope/config.toml or run Rope from a terminal"
        );
    }

    println!();
    println!("{}", "  Rope · first-time setup  ".black().on_cyan().bold());
    println!(
        "{}",
        "Configure one or more OpenAI-compatible providers. Each discovered model stays bound to its endpoint."
            .dark_grey()
    );

    let mut providers = Vec::new();
    let mut summaries = Vec::new();
    let mut models = Vec::new();
    loop {
        let number = providers.len() + 1;
        section(&format!("Provider {number}"));
        let endpoint = normalize_endpoint(&edit_line(
            "API endpoint",
            "https://api.openai.com/v1/",
            false,
        )?)?;
        let suggested_name = provider_name(&endpoint, number);
        let name = loop {
            let name = edit_line("Provider name", &suggested_name, false)?;
            if name.trim().is_empty() {
                warning("Provider name cannot be empty.");
            } else if providers
                .iter()
                .any(|provider: &ProviderConfig| provider.name == name)
            {
                warning("That provider name is already in use.");
            } else {
                break name;
            }
        };
        let api_key = edit_line("API key (optional)", "", true)?;

        print!("  {} Querying {endpoint}/models ... ", "●".yellow());
        io::stdout().flush()?;
        let mut discovered = OpenAiProvider::new(endpoint.clone(), api_key.clone())
            .models()
            .await?
            .into_iter()
            .filter(|id| supports_chat_completions(id))
            .map(|id| {
                let mut model = model_config(id);
                model.provider.clone_from(&name);
                model
            })
            .collect::<Vec<_>>();
        if is_openai_endpoint(&endpoint) {
            prioritize_openai_models(&mut discovered);
        }
        if discovered.is_empty() {
            bail!("the API returned no models usable with Chat Completions");
        }
        println!("{}", format!("{} chat model(s)", discovered.len()).green());

        summaries.push(ProviderSummary {
            name: name.clone(),
            endpoint: endpoint.clone(),
            models: discovered.len(),
        });
        providers.push(ProviderConfig {
            name,
            base_url: endpoint,
            api_key,
        });
        models.extend(discovered);

        if !confirm("Configure another provider?", false)? {
            break;
        }
    }

    disambiguate_model_names(&mut models);
    section("Browser tools");
    let browser = prepare_browser().await;

    let mut config = Config::default();
    config.base_url.clear();
    config.api_key.clear();
    config.providers = providers;
    config.model = models[0].name.clone();
    config.reasoning_effort = models[0].reasoning_effort;
    config.models = models;
    let path = config.write_global()?;

    report(&path, &config, &summaries, &browser)?;
    wait_for_enter()?;
    Ok(())
}

fn edit_line(label: &str, default: &str, secret: bool) -> Result<String> {
    let _mode = InputMode::enter()?;
    let mut stdout = io::stdout();
    let mut value = default.chars().collect::<Vec<_>>();
    let mut cursor = value.len();

    loop {
        draw_input(&mut stdout, label, &value, cursor, secret)?;
        match event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Enter => {
                        write!(stdout, "\r\n")?;
                        stdout.flush()?;
                        return Ok(value.into_iter().collect());
                    }
                    KeyCode::Esc => bail!("setup cancelled"),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        bail!("setup cancelled")
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        cursor = 0
                    }
                    KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        cursor = value.len()
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        value.drain(..cursor);
                        cursor = 0;
                    }
                    KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let start = previous_word(&value, cursor);
                        value.drain(start..cursor);
                        cursor = start;
                    }
                    KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        value.insert(cursor, character);
                        cursor += 1;
                    }
                    KeyCode::Backspace if cursor > 0 => {
                        cursor -= 1;
                        value.remove(cursor);
                    }
                    KeyCode::Delete if cursor < value.len() => {
                        value.remove(cursor);
                    }
                    KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                        cursor = previous_word(&value, cursor)
                    }
                    KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                        cursor = next_word(&value, cursor)
                    }
                    KeyCode::Left => cursor = cursor.saturating_sub(1),
                    KeyCode::Right => cursor = (cursor + 1).min(value.len()),
                    KeyCode::Home | KeyCode::Up => cursor = 0,
                    KeyCode::End | KeyCode::Down => cursor = value.len(),
                    _ => {}
                }
            }
            Event::Paste(text) => {
                let pasted = text
                    .chars()
                    .filter(|character| !character.is_control())
                    .collect::<Vec<_>>();
                value.splice(cursor..cursor, pasted.iter().copied());
                cursor += pasted.len();
            }
            _ => {}
        }
    }
}

fn draw_input(
    stdout: &mut io::Stdout,
    label: &str,
    value: &[char],
    cursor: usize,
    secret: bool,
) -> Result<()> {
    let prompt = format!("  {label}: ");
    let shown = if secret {
        "•".repeat(value.len())
    } else {
        value.iter().collect()
    };
    let column = prompt
        .chars()
        .count()
        .saturating_add(cursor)
        .min(u16::MAX as usize) as u16;
    queue!(
        stdout,
        MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(Color::Cyan),
        Print(&prompt),
        ResetColor,
        Print(shown),
        MoveToColumn(column)
    )?;
    stdout.flush()?;
    Ok(())
}

fn confirm(prompt: &str, default: bool) -> Result<bool> {
    let _mode = InputMode::enter()?;
    let mut stdout = io::stdout();
    let mut selected = default;
    loop {
        let yes = if selected {
            " Yes ".black().on_cyan().bold().to_string()
        } else {
            " Yes ".dark_grey().to_string()
        };
        let no = if selected {
            " No ".dark_grey().to_string()
        } else {
            " No ".black().on_cyan().bold().to_string()
        };
        queue!(
            stdout,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Print(format!("  {prompt}  {yes} {no}"))
        )?;
        stdout.flush()?;

        if let Event::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            match key.code {
                KeyCode::Enter => return finish_choice(&mut stdout, selected),
                KeyCode::Esc => bail!("setup cancelled"),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    bail!("setup cancelled")
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    return finish_choice(&mut stdout, true);
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    return finish_choice(&mut stdout, false);
                }
                KeyCode::Left | KeyCode::Up | KeyCode::Home => selected = true,
                KeyCode::Right | KeyCode::Down | KeyCode::End | KeyCode::Tab => selected = false,
                _ => {}
            }
        }
    }
}

fn finish_choice(stdout: &mut io::Stdout, value: bool) -> Result<bool> {
    write!(stdout, "\r\n")?;
    stdout.flush()?;
    Ok(value)
}

fn wait_for_enter() -> Result<()> {
    let _mode = InputMode::enter()?;
    let mut stdout = io::stdout();
    queue!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print("  Press Enter to start Rope"),
        ResetColor
    )?;
    stdout.flush()?;
    loop {
        if let Event::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            match key.code {
                KeyCode::Enter => {
                    write!(stdout, "\r\n")?;
                    stdout.flush()?;
                    return Ok(());
                }
                KeyCode::Esc => bail!("setup finished; start Rope again when ready"),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    bail!("setup finished; start Rope again when ready")
                }
                _ => {}
            }
        }
    }
}

fn previous_word(value: &[char], mut cursor: usize) -> usize {
    while cursor > 0 && !value[cursor - 1].is_alphanumeric() {
        cursor -= 1;
    }
    while cursor > 0 && value[cursor - 1].is_alphanumeric() {
        cursor -= 1;
    }
    cursor
}

fn next_word(value: &[char], mut cursor: usize) -> usize {
    while cursor < value.len() && value[cursor].is_alphanumeric() {
        cursor += 1;
    }
    while cursor < value.len() && !value[cursor].is_alphanumeric() {
        cursor += 1;
    }
    cursor
}

fn normalize_endpoint(endpoint: &str) -> Result<String> {
    let url = Url::parse(endpoint).context("invalid API endpoint")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("API endpoint must be an HTTP(S) URL");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("API endpoint cannot contain a query or fragment");
    }
    Ok(endpoint.trim_end_matches('/').to_owned())
}

fn provider_name(endpoint: &str, number: usize) -> String {
    let url = Url::parse(endpoint).unwrap();
    let host = url.host_str().unwrap_or("provider");
    if host.eq_ignore_ascii_case("api.openai.com") {
        "OpenAI".into()
    } else if host == "localhost" || host == "127.0.0.1" {
        format!("local-{number}")
    } else {
        host.split('.').next().unwrap_or("provider").to_owned()
    }
}

fn is_openai_endpoint(endpoint: &str) -> bool {
    Url::parse(endpoint).is_ok_and(|url| {
        url.host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.openai.com"))
    })
}

fn disambiguate_model_names(models: &mut [crate::config::ModelConfig]) {
    let counts = models.iter().fold(HashMap::new(), |mut counts, model| {
        *counts.entry(model.name.clone()).or_insert(0usize) += 1;
        counts
    });
    for model in models {
        if counts[&model.name] > 1 {
            model.name = format!("{}/{}", model.provider, model.id);
        }
    }
}

async fn prepare_browser() -> BrowserSummary {
    print!("  {} Preparing Patchright runtime ... ", "●".yellow());
    io::stdout().flush().ok();
    let runtime_ready = match crate::tool::prepare_browser_runtime().await {
        Ok(Some(_)) => {
            println!("{}", "ready".green());
            true
        }
        Ok(None) => {
            println!("{}", "not embedded in this build".red());
            false
        }
        Err(error) => {
            println!("{}", format!("failed: {error:#}").red());
            false
        }
    };
    let executable = crate::tool::browser_executable();
    match &executable {
        Some(path) => success(&format!("Chrome-compatible browser: {}", path.display())),
        None => {
            warning("Chrome-compatible browser not found. Web tools are unavailable.");
            println!(
                "  {}",
                "Set one explicitly: export ROPE_BROWSER=/path/to/chrome".yellow()
            );
        }
    }
    BrowserSummary {
        executable,
        runtime_ready,
    }
}

fn report(
    config_path: &std::path::Path,
    config: &Config,
    providers: &[ProviderSummary],
    browser: &BrowserSummary,
) -> Result<()> {
    section("Setup report");
    success(&format!("Saved {}", config_path.display()));
    for provider in providers {
        println!(
            "  {} {}  {}  {}",
            "◆".cyan(),
            provider.name.clone().bold(),
            provider.endpoint.clone().dark_grey(),
            format!("{} model(s)", provider.models).green()
        );
    }
    println!(
        "  {} Default model: {}",
        "◆".cyan(),
        config.model.clone().bold()
    );

    let web_available = browser.runtime_ready && browser.executable.is_some();
    if web_available {
        success(
            "Tools: read, write, edit, shell, grep, glob, view_image, update_plan, web_search, web_browser",
        );
    } else {
        success("Tools: read, write, edit, shell, grep, glob, view_image, update_plan");
        warning("Web tools: unavailable until both Patchright and Chrome are available");
    }

    section("Configuration reference");
    let root = config_root()?;
    let cwd = std::env::current_dir()?;
    reference("Global config", root.join("config.toml"));
    reference("Local config", cwd.join(".rope/config.toml"));
    reference("Global AGENTS", root.join("AGENTS.md"));
    reference("Local AGENTS", cwd.join("AGENTS.md"));
    reference("Global tools", root.join("tools"));
    reference("Local tools", cwd.join(".rope/tools"));
    println!();
    Ok(())
}

fn section(title: &str) {
    println!();
    println!("{} {}", "──".dark_grey(), title.cyan().bold());
}

fn success(message: &str) {
    println!("  {} {message}", "✓".green().bold());
}

fn warning(message: &str) {
    println!("  {} {message}", "!".yellow().bold());
}

fn reference(label: &str, path: PathBuf) {
    println!(
        "  {:<15} {}",
        label.to_owned().dark_grey(),
        path.display().to_string().cyan()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_rejects_non_http_urls() {
        assert!(normalize_endpoint("file:///tmp/api").is_err());
        assert_eq!(
            normalize_endpoint("https://example.com/v1/").unwrap(),
            "https://example.com/v1"
        );
    }

    #[test]
    fn official_endpoint_is_named_openai() {
        assert_eq!(provider_name("https://api.openai.com/v1", 1), "OpenAI");
        assert!(is_openai_endpoint("https://api.openai.com/v1"));
    }

    #[test]
    fn duplicate_model_ids_get_provider_qualified_names() {
        let mut models = vec![
            model_config("same-model".into()),
            model_config("same-model".into()),
        ];
        models[0].provider = "one".into();
        models[1].provider = "two".into();

        disambiguate_model_names(&mut models);

        assert_eq!(models[0].name, "one/same-model");
        assert_eq!(models[1].name, "two/same-model");
    }

    #[test]
    fn word_navigation_uses_shell_boundaries() {
        let value = "hello/api world".chars().collect::<Vec<_>>();
        assert_eq!(previous_word(&value, value.len()), 10);
        assert_eq!(next_word(&value, 0), 6);
    }
}
