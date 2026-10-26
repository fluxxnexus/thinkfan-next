//! Subscriber configuration belongs to the binary; library code only emits events.
use std::{
    env, fmt,
    io::IsTerminal,
    os::{fd::AsFd, unix::fs::MetadataExt},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{Event, Subscriber, level_filters::LevelFilter};
use tracing_subscriber::{
    EnvFilter,
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    prelude::*,
    registry::LookupSpan,
};

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum LogOutput {
    #[default]
    Auto,
    Console,
    Journal,
}

fn journal_stream_matches(stream: &str, metadata: &std::fs::Metadata) -> bool {
    let Some((device, inode)) = stream.split_once(':') else {
        return false;
    };
    device.parse::<u64>().ok() == Some(metadata.dev())
        && inode.parse::<u64>().ok() == Some(metadata.ino())
}

fn stderr_is_journal() -> bool {
    let Ok(stream) = env::var("JOURNAL_STREAM") else {
        return false;
    };
    // Match the actual descriptor: inherited environment variables alone are
    // insufficient when a shell or wrapper redirects stderr elsewhere.
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .ok()
        .and_then(|fd| std::fs::File::from(fd).metadata().ok())
        .is_some_and(|metadata| journal_stream_matches(&stream, &metadata))
}

fn journal_layer() -> std::io::Result<tracing_journald::Layer> {
    use tracing_journald::{Priority, PriorityMappings};
    Ok(tracing_journald::layer()?
        .with_syslog_identifier("thinkfan-next".to_owned())
        .with_priority_mappings(PriorityMappings {
            error: Priority::Error,
            warn: Priority::Warning,
            info: Priority::Informational,
            debug: Priority::Debug,
            trace: Priority::Debug,
        }))
}

/// Human-readable stderr output. Event fields remain available without repeating
/// the controller span and configuration path on every line.
struct ConsoleFormat {
    detailed: bool,
    color: bool,
}
impl<S, N> FormatEvent<S, N> for ConsoleFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        write_local_time(&mut writer)?;
        let metadata = event.metadata();
        let level = metadata.level();
        if self.color {
            let color = match *level {
                tracing::Level::ERROR => 31,
                tracing::Level::WARN => 33,
                tracing::Level::INFO => 32,
                tracing::Level::DEBUG => 34,
                tracing::Level::TRACE => 35,
            };
            write!(writer, " \x1b[{color}m{level:>5}\x1b[0m ")?;
        } else {
            write!(writer, " {level:>5} ")?;
        }
        if self.detailed || *level == tracing::Level::DEBUG || *level == tracing::Level::TRACE {
            write!(writer, "{}: ", metadata.target())?;
        }
        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

fn write_local_time(writer: &mut Writer<'_>) -> fmt::Result {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| fmt::Error)?
        .as_secs();
    let seconds: libc::time_t = seconds.try_into().map_err(|_| fmt::Error)?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // localtime_r writes to caller-owned storage and does not share a static tm.
    let local = unsafe {
        if libc::localtime_r(&seconds, local.as_mut_ptr()).is_null() {
            return Err(fmt::Error);
        }
        local.assume_init()
    };
    let mut buffer = [0u8; 64];
    // strftime writes at most buffer.len() bytes using the initialized tm.
    let len = unsafe {
        libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            c"%Y-%m-%d %H:%M:%S %z".as_ptr(),
            &local,
        )
    };
    if len < 5 {
        return Err(fmt::Error);
    }
    // strftime's portable %z uses ±HHMM; display the offset as ±HH:MM.
    let prefix = std::str::from_utf8(&buffer[..len - 2]).map_err(|_| fmt::Error)?;
    let minutes = std::str::from_utf8(&buffer[len - 2..len]).map_err(|_| fmt::Error)?;
    write!(writer, "{prefix}:{minutes}")
}

pub fn init(verbosity: u8, output: LogOutput) {
    let level = match verbosity {
        0 => LevelFilter::INFO,
        1 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    let default_filter = || EnvFilter::new(level.to_string());
    let (filter, warning) = match env::var("RUST_LOG") {
        Ok(directives) => match EnvFilter::builder()
            .with_default_directive(level.into())
            .parse(directives)
        {
            Ok(filter) => (filter, None),
            Err(error) => (default_filter(), Some(error.to_string())),
        },
        Err(env::VarError::NotPresent) => (default_filter(), None),
        Err(error) => (default_filter(), Some(error.to_string())),
    };
    let use_journal = match output {
        LogOutput::Auto => stderr_is_journal(),
        LogOutput::Console => false,
        LogOutput::Journal => true,
    };
    let (journal, journal_error) = if use_journal {
        match journal_layer() {
            Ok(layer) => (Some(layer), None),
            Err(error) => (None, Some(error)),
        }
    } else {
        (None, None)
    };
    let console = journal.is_none().then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .event_format(ConsoleFormat {
                detailed: verbosity > 0,
                color: std::io::stderr().is_terminal() && env::var_os("NO_COLOR").is_none(),
            })
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(journal)
        .with(console)
        .init();
    if let Some(error) = journal_error {
        tracing::warn!(%error, "journal logging unavailable; falling back to stderr");
    }
    if let Some(error) = warning {
        tracing::warn!(%error, "invalid RUST_LOG; using verbosity default");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_detection_requires_matching_descriptor_identity() {
        let file = tempfile::tempfile().unwrap();
        let metadata = file.metadata().unwrap();
        let matching = format!("{}:{}", metadata.dev(), metadata.ino());
        assert!(journal_stream_matches(&matching, &metadata));
        for invalid in [
            "".to_owned(),
            "journal".to_owned(),
            "1:2:3".to_owned(),
            format!("{}:{}", metadata.dev(), metadata.ino().wrapping_add(1)),
            format!("{}:{}", metadata.dev().wrapping_add(1), metadata.ino()),
        ] {
            assert!(!journal_stream_matches(&invalid, &metadata), "{invalid}");
        }
    }
}
