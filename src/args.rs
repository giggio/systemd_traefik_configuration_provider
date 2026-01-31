use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, PartialEq)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub verbosity: clap_verbosity_flag::Verbosity,

    #[arg(short = 'd', long, env = "TRAEFIK_LOG_HIDE_DATE", global = true)]
    pub log_hide_date: bool,

    #[arg(
        short,
        long,
        value_name = "FILE",
        env = "TRAEFIK_OUT_DIR",
        default_value = "/etc/traefik/dynamic/units",
        global = true
    )]
    pub traefik_out_dir: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use pretty_assertions::assert_eq;

    const BASIC_ARGS: [&str; 1] = ["systemd_traefik_configuration_provider"];

    #[test]
    fn test_cli_basic_parsing() {
        let args = Vec::from(BASIC_ARGS);

        let cli = Cli::parse_from(args);
        assert_eq!(
            "/etc/traefik/dynamic/units",
            cli.traefik_out_dir.to_str().unwrap()
        );
    }

    #[test]
    fn test_cli_with_traefik_out_dir() {
        let args = Vec::from(BASIC_ARGS)
            .into_iter()
            .chain(vec!["--traefik-out-dir", "/tmp/traefik"])
            .collect::<Vec<_>>();
        let cli = Cli::parse_from(args);
        assert_eq!(cli.traefik_out_dir, PathBuf::from("/tmp/traefik"));
    }
}
