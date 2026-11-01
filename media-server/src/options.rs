use clap::{Parser, builder::TypedValueParser};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

/// Realtime media streaming and live-rewind DVR server.
#[derive(Debug, Parser)]
#[command(
    name = "media-server",
    version,
    about,
    after_help = "Relative paths use the working directory. Recovery always runs at startup.
Retention starts immediately when enabled. BIND_ADDRESS, DVR_RETENTION_SECS and
RUST_LOG do not configure this binary; use the corresponding flags instead."
)]
pub struct Options {
    /// IPv4 or IPv6 listen address (without the port)
    #[arg(long, value_name = "IP", default_value = "0.0.0.0")]
    pub bind_address: IpAddr,
    /// Listen port; 0 selects a free port
    #[arg(long, value_name = "PORT", default_value_t = 8009)]
    pub port: u16,
    /// Recording, playback, recovery and retention directory
    #[arg(long, value_name = "PATH", default_value = "dvr", value_parser = path_parser())]
    pub dvr_dir: PathBuf,
    /// Recording age limit in seconds; 0 disables automatic deletion
    #[arg(
        long = "dvr-retention-secs",
        value_name = "SECONDS",
        default_value_t = 0
    )]
    pub retention_secs: u64,
    /// Positive interval between retention cleanup runs
    #[arg(long = "retention-check-interval-secs", value_name = "SECONDS", default_value = "60", value_parser = parse_interval)]
    pub retention_interval_secs: u64,
    /// Daily log directory (logs are also written to stdout)
    #[arg(long, value_name = "PATH", default_value = "logs", value_parser = path_parser())]
    pub log_dir: PathBuf,
    /// Tracing filter, e.g. info,media_server=debug
    #[arg(long, value_name = "FILTER", default_value = "info", value_parser = parse_log_filter)]
    pub log_filter: String,
}
impl Options {
    pub fn address(&self) -> SocketAddr {
        SocketAddr::new(self.bind_address, self.port)
    }
    pub fn retention_seconds(&self) -> Option<u64> {
        (self.retention_secs > 0).then_some(self.retention_secs)
    }
}
fn path_parser() -> impl TypedValueParser<Value = PathBuf> {
    clap::builder::PathBufValueParser::new().try_map(|path| {
        if path.as_os_str().is_empty() {
            Err("Path must not be empty")
        } else {
            Ok(path)
        }
    })
}
fn parse_interval(value: &str) -> Result<u64, String> {
    let seconds: u64 = value
        .parse()
        .map_err(|_| "Expected a positive number of seconds")?;
    if seconds == 0
        || std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(seconds))
            .is_none()
    {
        return Err("Interval must be positive and fit the monotonic clock".into());
    }
    Ok(seconds)
}
fn parse_log_filter(value: &str) -> Result<String, String> {
    tracing_subscriber::EnvFilter::try_new(value).map_err(|error| error.to_string())?;
    Ok(value.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Result<Options, clap::Error> {
        Options::try_parse_from(std::iter::once("media-server").chain(values.iter().copied()))
    }
    #[test]
    fn defaults_disable_retention() {
        let options = args(&[]).unwrap();
        assert_eq!(options.address().to_string(), "0.0.0.0:8009");
        assert_eq!(options.dvr_dir, PathBuf::from("dvr"));
        assert_eq!(options.log_dir, PathBuf::from("logs"));
        assert_eq!(options.log_filter, "info");
        assert_eq!(options.retention_seconds(), None);
        assert_eq!(options.retention_interval_secs, 60);
    }
    #[test]
    fn all_options_support_explicit_values_and_ipv6() {
        let options = args(&[
            "--bind-address",
            "::1",
            "--port=8010",
            "--dvr-dir",
            "/tmp/my recordings",
            "--dvr-retention-secs=604800",
            "--retention-check-interval-secs",
            "30",
            "--log-dir=/tmp/my logs",
            "--log-filter",
            "media_server=debug",
        ])
        .unwrap();
        assert_eq!(options.address().to_string(), "[::1]:8010");
        assert_eq!(options.dvr_dir, PathBuf::from("/tmp/my recordings"));
        assert_eq!(options.log_dir, PathBuf::from("/tmp/my logs"));
        assert_eq!(options.retention_seconds(), Some(604800));
        assert_eq!(options.retention_interval_secs, 30);
        assert_eq!(options.log_filter, "media_server=debug");
        let options = args(&["--dvr-retention-secs=0", "--port=0"]).unwrap();
        assert_eq!(options.retention_seconds(), None);
        assert_eq!(options.address().port(), 0);
    }
    #[test]
    fn malformed_options_fail_instead_of_silently_using_defaults() {
        for values in [
            &["--unknown"][..],
            &["--port"],
            &["--port", "--help"],
            &["--port=-1"],
            &["--port=65536"],
            &["--bind-address=localhost"],
            &["--dvr-retention-secs=bad"],
            &["--retention-check-interval-secs=0"],
            &["--log-filter=[bad"],
            &["--dvr-dir="],
            &["--port=1", "--port=2"],
        ] {
            assert!(args(values).is_err(), "accepted {values:?}");
        }
    }
    #[test]
    fn help_and_version_do_not_require_startup() {
        for option in ["--help", "-h"] {
            assert_eq!(
                args(&[option]).unwrap_err().kind(),
                clap::error::ErrorKind::DisplayHelp
            );
        }
        for option in ["--version", "-V"] {
            assert_eq!(
                args(&[option]).unwrap_err().kind(),
                clap::error::ErrorKind::DisplayVersion
            );
        }
    }
}
