use std::io::IsTerminal;

use crate::output::OutputFormat;

const ASCII_BANNER: &str = concat!(
    " ██╗   ██╗ ██████╗ ██╗██████╗        ██████╗  ██████╗ ██╗  ██╗\n",
    " ██║   ██║██╔═══██╗██║██╔══██╗       ██╔══██╗██╔═══██╗╚██╗██╔╝\n",
    " ██║   ██║██║   ██║██║██║  ██║       ██████╔╝██║   ██║ ╚███╔╝\n",
    " ╚██╗ ██╔╝██║   ██║██║██║  ██║█████╗ ██╔══██╗██║   ██║ ██╔██╗\n",
    "  ╚████╔╝ ╚██████╔╝██║██████╔╝╚════╝ ██████╔╝╚██████╔╝██╔╝ ██╗\n",
    "   ╚═══╝   ╚═════╝ ╚═╝╚═════╝        ╚═════╝  ╚═════╝ ╚═╝  ╚═╝",
);

/// Embedded fallback for the TUI logo (same as the run banner).
const EMBEDDED_LOGO: &str = "⬢ VOID-BOX";

/// Determine whether the ASCII banner should be shown.
///
/// Show only when:
/// - subcommand is `run` (caller decides)
/// - output format is human
/// - `--no-banner` was not passed
/// - `VOIDBOX_NO_BANNER` is not set
/// - stderr is a terminal (interactive)
/// - stdin is a terminal (not piped)
pub fn should_show_banner(output: OutputFormat, config_banner: bool) -> bool {
    should_show_banner_inner(
        output,
        config_banner,
        std::io::stderr().is_terminal(),
        std::io::stdin().is_terminal(),
    )
}

fn should_show_banner_inner(
    output: OutputFormat,
    config_banner: bool,
    stderr_is_tty: bool,
    stdin_is_tty: bool,
) -> bool {
    if output == OutputFormat::Json {
        return false;
    }
    if !config_banner {
        return false;
    }
    if !stderr_is_tty {
        return false;
    }
    if !stdin_is_tty {
        return false;
    }
    true
}

/// Print the startup banner for `run` on stderr.
pub fn print_startup_banner(sandbox: &void_box::spec::SandboxSpec) {
    let version = env!("CARGO_PKG_VERSION");
    let net = if sandbox.network { "on" } else { "off" };
    let mut summary = format!(
        "  {}MB RAM · {} vCPUs · network={}",
        sandbox.memory_mb, sandbox.vcpus, net
    );
    if sandbox.image.is_some() {
        summary.push_str(" · oci=yes");
    }
    if std::io::stderr().is_terminal() {
        eprintln!(
            "\x1b[38;5;153m{}  v{}\n\n{}\x1b[0m\n",
            ASCII_BANNER, version, summary
        );
    } else {
        eprintln!("{}  v{}\n\n{}\n", ASCII_BANNER, version, summary);
    }
}

/// Resolve and print the TUI logo header.
///
/// Resolution order:
/// 1. `--logo-ascii` CLI flag
/// 2. `VOIDBOX_LOGO_ASCII_PATH` env
/// 3. `/usr/share/voidbox/logo.txt` (packaged)
/// 4. Embedded fallback
pub fn print_logo_header(logo_cli: Option<&str>) {
    let candidates: Vec<Option<String>> = vec![
        logo_cli.map(String::from),
        std::env::var("VOIDBOX_LOGO_ASCII_PATH").ok(),
        Some("/usr/share/voidbox/logo.txt".into()),
    ];

    for candidate in candidates.into_iter().flatten() {
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            if !text.trim().is_empty() {
                println!("{text}");
                return;
            }
        }
    }

    println!("{EMBEDDED_LOGO}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_suppresses_banner() {
        assert!(!should_show_banner(OutputFormat::Json, true));
    }

    #[test]
    fn no_banner_config_suppresses_banner() {
        assert!(!should_show_banner(OutputFormat::Human, false));
    }

    #[test]
    fn banner_suppressed_when_either_stdio_not_tty() {
        // Do not rely on real TTY detection (IDE/CI may attach a PTY).
        assert!(!should_show_banner_inner(
            OutputFormat::Human,
            true,
            false,
            true
        ));
        assert!(!should_show_banner_inner(
            OutputFormat::Human,
            true,
            true,
            false
        ));
        assert!(!should_show_banner_inner(
            OutputFormat::Human,
            true,
            false,
            false
        ));
    }

    #[test]
    fn banner_allowed_when_human_and_both_tty() {
        assert!(should_show_banner_inner(
            OutputFormat::Human,
            true,
            true,
            true
        ));
    }
}
