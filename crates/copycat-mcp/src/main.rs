//! copycat-mcp — the stdio MCP server Claude (and OpenCode) connect to. Every
//! tool is a thin proxy onto `copycatd` over its local socket, so the model
//! shares the exact clipboard the user's Ctrl+C / Ctrl+V touches.

mod proxy;
mod server;

use rmcp::transport::stdio;
use rmcp::ServiceExt;

use copycat_core::proto::socket_path;

use crate::proxy::Daemon;
use crate::server::Copycat;

#[derive(Debug, PartialEq, Eq)]
enum ModeAction {
    Version,
    Help,
    Stdio,
    Unknown(String),
}

fn parse_mode(arg: Option<&str>) -> ModeAction {
    match arg.unwrap_or("--stdio") {
        "--version" | "-V" => ModeAction::Version,
        "--help" | "-h" => ModeAction::Help,
        "--stdio" => ModeAction::Stdio,
        other => ModeAction::Unknown(other.to_string()),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // MCP stdio uses stdout for protocol traffic, so logs MUST go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("COPYCAT_MCP_LOG")
                .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
                .unwrap_or_else(|_| "copycat_mcp=info".into()),
        )
        .init();

    if copycat_core::insecure::reveal_secrets() {
        tracing::warn!(
            "INSECURE DEBUG MODE active: Secret/PII clipboard values will be returned \
             to the model in cleartext. Never use with real data."
        );
    }

    let mode_arg = std::env::args().nth(1);
    match parse_mode(mode_arg.as_deref()) {
        ModeAction::Version => {
            println!("copycat-mcp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        ModeAction::Help => {
            println!(
                "copycat-mcp {}\n\
                 Model Context Protocol (MCP) server for copycat.\n\n\
                 USAGE:\n    copycat-mcp [OPTIONS]\n\n\
                 OPTIONS:\n    \
                 --stdio         Run in stdio mode for local MCP clients (default)\n    \
                 -V, --version   Print version information\n    \
                 -h, --help      Print help information",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        ModeAction::Stdio => {
            let daemon = Daemon::new(socket_path());
            run_stdio(daemon).await
        }
        ModeAction::Unknown(other) => {
            anyhow::bail!("unknown mode {other:?}; use --stdio, --version, or --help")
        }
    }
}

async fn run_stdio(daemon: Daemon) -> anyhow::Result<()> {
    tracing::info!(
        "copycat-mcp starting (stdio) — proxying to {}",
        daemon.path().display()
    );
    let service = Copycat::new(daemon).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_flag_resolution() {
        assert_eq!(parse_mode(None), ModeAction::Stdio);
        assert_eq!(parse_mode(Some("--stdio")), ModeAction::Stdio);
        assert_eq!(
            parse_mode(Some("--http")),
            ModeAction::Unknown("--http".into())
        );
        assert_eq!(parse_mode(Some("--version")), ModeAction::Version);
        assert_eq!(parse_mode(Some("-V")), ModeAction::Version);
        assert_eq!(parse_mode(Some("--help")), ModeAction::Help);
        assert_eq!(parse_mode(Some("-h")), ModeAction::Help);
        assert_eq!(
            parse_mode(Some("--invalid")),
            ModeAction::Unknown("--invalid".into())
        );
    }
}
