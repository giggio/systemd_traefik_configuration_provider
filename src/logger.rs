use flexi_logger::{
    AdaptiveFormat, DeferredNow, Logger, LoggerHandle, TS_DASHES_BLANK_COLONS_DOT_BLANK, style,
};
use log::{LevelFilter, Record};
use std::{env, io::IsTerminal};

pub type Result<T> = std::result::Result<T, Error>;

pub fn start(level_filter: LevelFilter) -> Result<LoggerHandle> {
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
        logger = logger.adaptive_format_for_stdout(AdaptiveFormat::Detailed);
    } else {
        logger = logger.format(if std::io::stdout().is_terminal() {
            colored_detailed_format
        } else {
            detailed_format
        });
    }
    let logger_handle = logger.start()?;
    if cargo_run {
        warn!("Running from cargo...");
    }
    Ok(logger_handle)
}

// adapted from flexi_logger:
fn detailed_format(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::result::Result<(), std::io::Error> {
    write!(
        w,
        "[{}] {} [{}]: ",
        now.format(TS_DASHES_BLANK_COLONS_DOT_BLANK),
        record.level(),
        record.module_path().unwrap_or("<unnamed>"),
    )?;

    write_key_value_pairs(w, record)?;

    write!(w, "{}", &record.args())
}
fn colored_detailed_format(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::result::Result<(), std::io::Error> {
    let level = record.level();
    write!(
        w,
        "[{}] {} [{}]: ",
        style(level).paint(now.format(TS_DASHES_BLANK_COLONS_DOT_BLANK).to_string()),
        style(level).paint(record.level().to_string()),
        record.module_path().unwrap_or("<unnamed>"),
    )?;
    write_key_value_pairs(w, record)?;
    write!(w, "{}", style(level).paint(record.args().to_string()))
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
