use flexi_logger::{
    AdaptiveFormat, DeferredNow, Logger, LoggerHandle, TS_DASHES_BLANK_COLONS_DOT_BLANK, style,
};
use log::{LevelFilter, Record};
use std::{env, io::IsTerminal};

pub type Result<T> = std::result::Result<T, Error>;

pub fn start(level_filter: LevelFilter, hide_date: bool) -> Result<LoggerHandle> {
    let mut logger = Logger::try_with_env_or_str(level_filter.as_str())?
        .log_to_stdout()
        .set_palette("9;11;15;14;12".to_owned());
    #[cfg(test)]
    {
        logger = logger.write_mode(flexi_logger::WriteMode::SupportCapture);
    }
    #[allow(unused_mut)]
    let mut cargo_run = false;
    if env::var("CARGO_MANIFEST_DIR").is_ok() {
        #[cfg(not(test))]
        {
            cargo_run = true;
        }
        logger = logger.adaptive_format_for_stdout(AdaptiveFormat::Detailed); // shows line numbers
    } else {
        let format: flexi_logger::FormatFunction =
            match (std::io::stdout().is_terminal(), hide_date) {
                (true, false) => detailed_format::<true, true>,
                (true, true) => detailed_format::<false, true>,
                (false, false) => detailed_format::<true, false>,
                (false, true) => detailed_format::<false, false>,
            };
        logger = logger.format(format);
    }
    let logger_handle = logger.start()?;
    if cargo_run {
        warn!("Running from cargo...");
    }
    Ok(logger_handle)
}

// adapted from flexi_logger. The flags are const generics because flexi_logger takes a plain
// function pointer, so each combination is its own function.
fn detailed_format<const SHOW_DATE: bool, const COLORED: bool>(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::result::Result<(), std::io::Error> {
    let level = record.level();
    let paint = |text: String| {
        if COLORED {
            style(level).paint(text).to_string()
        } else {
            text
        }
    };
    if SHOW_DATE {
        write!(
            w,
            "[{}] ",
            paint(now.format(TS_DASHES_BLANK_COLONS_DOT_BLANK).to_string())
        )?;
    }
    write!(
        w,
        "{} [{}]: ",
        paint(level.to_string()),
        record.module_path().unwrap_or("<unnamed>"),
    )?;
    write_key_value_pairs(w, record)?;
    write!(w, "{}", paint(record.args().to_string()))
}

// originally from flexi_logger:
fn write_key_value_pairs(
    w: &mut dyn std::io::Write,
    record: &Record<'_>,
) -> std::result::Result<(), std::io::Error> {
    if record.key_values().count() > 0 {
        write!(w, "{{")?;
        let mut kv_stream = KvStream(w, false);
        record.key_values().visit(&mut kv_stream).ok();
        write!(w, "}} ")?;
    }
    Ok(())
}
struct KvStream<'a>(&'a mut dyn std::io::Write, bool);
impl<'kvs, 'a> log::kv::VisitSource<'kvs> for KvStream<'a>
where
    'kvs: 'a,
{
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> std::result::Result<(), log::kv::Error> {
        if self.1 {
            write!(self.0, ", ")?;
        }
        write!(self.0, "{key}={value:?}")?;
        self.1 = true;
        Ok(())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Logger(#[from] flexi_logger::FlexiLoggerError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn format_with(
        format: fn(
            &mut dyn std::io::Write,
            &mut DeferredNow,
            &Record,
        ) -> std::result::Result<(), std::io::Error>,
    ) -> String {
        let mut out = Vec::new();
        format(
            &mut out,
            &mut DeferredNow::new(),
            &Record::builder()
                .level(log::Level::Info)
                .module_path(Some("app::module"))
                .args(format_args!("hello"))
                .build(),
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn test_detailed_format_without_date() {
        assert_eq!(
            format_with(detailed_format::<false, false>),
            "INFO [app::module]: hello"
        );
    }

    #[test]
    fn test_detailed_format_with_date() {
        let formatted = format_with(detailed_format::<true, false>);
        assert!(formatted.starts_with('['), "{formatted}");
        assert!(
            formatted.ends_with("] INFO [app::module]: hello"),
            "{formatted}"
        );
    }

    #[test]
    fn test_detailed_format_colored_keeps_the_text() {
        let formatted = format_with(detailed_format::<false, true>);
        assert!(formatted.contains("INFO"), "{formatted}");
        assert!(formatted.contains(" [app::module]: "), "{formatted}");
        assert!(formatted.contains("hello"), "{formatted}");
    }
}
