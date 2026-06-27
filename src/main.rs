use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand};
use claude_code_proxy::{
    config, paths,
    registry::{ANTHROPIC_STYLE_ALIASES, Registry},
    server::{self, ServerConfig},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(
    name = "claude-code-proxy",
    version = VERSION,
    about = "Anthropic-compatible proxy for Claude Code provider backends",
    disable_version_flag = true
)]
struct Cli {
    #[arg(long = "version", short = 'v', action = ArgAction::SetTrue)]
    version_flag: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Version,
    Serve {
        #[arg(long)]
        port: Option<u16>,
    },
    Models {
        #[arg(long)]
        full: bool,
    },
    Codex {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    Kimi {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    Cursor {
        #[command(subcommand)]
        command: ProviderGroup,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderGroup {
    Auth {
        #[command(subcommand)]
        command: claude_code_proxy::provider::AuthCommand,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.version_flag {
        println!("claude-code-proxy {}", VERSION);
        return Ok(());
    }

    let commands = cli.command.unwrap_or(Commands::Serve { port: None });

    match commands {
        Commands::Version => {
            println!("claude-code-proxy {}", VERSION);
            Ok(())
        }
        Commands::Serve { port } => {
            let effective_port = port.unwrap_or_else(config::port);
            let registry = Registry::with_default_alias();
            print_server_banner(effective_port, &registry);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(server::serve(ServerConfig {
                    port: effective_port,
                }))
                .map_err(|err| anyhow::anyhow!(err))
        }
        Commands::Models { full } => {
            print_models(&Registry::with_default_alias(), full);
            Ok(())
        }
        Commands::Codex { command } => run_provider_cli("codex", command),
        Commands::Kimi { command } => run_provider_cli("kimi", command),
        Commands::Cursor { command } => run_provider_cli("cursor", command),
    }
}

fn run_provider_cli(name: &str, command: ProviderGroup) -> Result<()> {
    let registry = Registry::with_default_alias();
    let provider = registry
        .provider(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
    let handlers = provider.cli();
    match command {
        ProviderGroup::Auth { command } => match command {
            claude_code_proxy::provider::AuthCommand::Login => {
                if let Err(err) = handlers.login() {
                    eprintln!("{err}");
                    std::process::exit(2);
                }
                Ok(())
            }
            claude_code_proxy::provider::AuthCommand::Device => {
                if let Err(err) = handlers.device() {
                    eprintln!("{err}");
                    std::process::exit(2);
                }
                Ok(())
            }
            claude_code_proxy::provider::AuthCommand::Status => {
                if let Err(err) = handlers.status() {
                    println!("{err}");
                    if err.to_string() == "Not authenticated" {
                        std::process::exit(1);
                    }
                    std::process::exit(2);
                }
                Ok(())
            }
            claude_code_proxy::provider::AuthCommand::Logout => {
                handlers.logout()?;
                Ok(())
            }
        },
    }
}

fn print_models(registry: &Registry, full: bool) {
    let grouped = registry.grouped_models();
    for provider in ["codex", "kimi", "cursor"] {
        let Some(models) = grouped.get(provider) else {
            continue;
        };
        if full || provider != "cursor" {
            println!("{provider}: {}", models.join(", "));
        } else {
            println!("{provider}: {}", compact_cursor_list(models));
        }
    }
}

fn compact_cursor_list(models: &[String]) -> String {
    let mut legacy = Vec::new();
    let mut dynamic = Vec::new();
    for model in models {
        if !model.contains(':') {
            legacy.push(model.clone());
        } else {
            dynamic.push(model.clone());
        }
    }
    let mut out = String::new();
    if !legacy.is_empty() {
        out.push_str(&legacy.join(", "));
        out.push_str("; ");
    }
    out.push_str(&format!("{} cursor model aliases", dynamic.len()));
    if !dynamic.is_empty() {
        out.push_str(", example: cursor:gpt-5.5");
    }
    out.push_str(" run `claude-code-proxy models --full` for all aliases");
    out
}

fn print_server_banner(port: u16, registry: &Registry) {
    println!("Proxy listening on http://127.0.0.1:{port}");
    println!("Logs: {}", paths::log_file().display());
    let cfg = paths::config_dir();
    if cfg.exists() {
        println!("Config: {}", cfg.display());
    }
    print_models(registry, false);
    println!();
    println!("Configure Claude Code (pick a model from above):");
    println!("  export ANTHROPIC_BASE_URL=\"http://localhost:{port}\"");
    println!("  export ANTHROPIC_AUTH_TOKEN=\"anything\"");
    println!("  export ANTHROPIC_MODEL=\"gpt-5.5\"");
    println!("  export ANTHROPIC_SMALL_FAST_MODEL=\"gpt-5.4-mini\"");
    println!("  export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1");
}

#[allow(dead_code)]
fn alias_names() -> usize {
    ANTHROPIC_STYLE_ALIASES.len()
}
