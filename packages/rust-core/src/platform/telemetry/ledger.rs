//! Append-only usage ledger: the background writer, safe file opening, replay and day arithmetic.

use std::{
    io::{BufRead, Write as _},
    path::Path,
    sync::{Mutex as StdMutex, mpsc},
};

use super::{Inner, LEDGER_MAX_BYTES, LedgerCommand, TaskRecord};

pub(crate) fn ledger_worker(
    path: &Path,
    inner: &StdMutex<Inner>,
    receiver: mpsc::Receiver<LedgerCommand>,
) {
    for command in receiver {
        match command {
            LedgerCommand::Record(record) => {
                let written = append(path, &record);
                let mut inner = inner.lock().expect("telemetry state poisoned");
                inner.ledger_pending -= 1;
                match written {
                    Ok(()) => inner.ledger_records += 1,
                    Err(error) => {
                        inner.ledger_failed += 1;
                        inner.ledger_error = Some(error.to_string());
                    }
                }
            }
            LedgerCommand::Flush(sender) => {
                let _ = sender.send(());
            }
        }
    }
}

fn require_regular(metadata: &std::fs::Metadata) -> std::io::Result<()> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Usage ledger must be a regular file.",
        ))
    }
}

/// Check both the path and opened handle. Nonblocking open prevents a FIFO substituted
/// between those checks from waiting for a reader/writer on Unix.
fn open_regular(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    match std::fs::metadata(path) {
        Ok(metadata) => require_regular(&metadata)?,
        Err(error) if append && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = std::fs::OpenOptions::new();
    if append {
        options.create(true).append(true);
    } else {
        options.read(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    require_regular(&file.metadata()?)?;
    Ok(file)
}

pub(crate) fn append(path: &Path, record: &TaskRecord) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::metadata(path) {
        Ok(metadata) => {
            require_regular(&metadata)?;
            if metadata.len() >= LEDGER_MAX_BYTES {
                let mut rotated = path.as_os_str().to_owned();
                rotated.push(".1");
                std::fs::rename(path, rotated)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut file = open_regular(path, true)?;
    file.write_all(line.as_bytes())
}

/// Read ledger records back; malformed lines are skipped, not fatal.
pub(crate) fn replay(path: &Path) -> std::io::Result<Vec<TaskRecord>> {
    let file = match open_regular(path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let reader = std::io::BufReader::new(std::io::Read::take(file, LEDGER_MAX_BYTES));
    Ok(reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<TaskRecord>(&line).ok())
        .collect())
}

pub(crate) fn unix_day(seconds: u64) -> u64 {
    seconds / 86_400
}

/// `YYYY-MM-DD` (UTC) for a day number since 1970-01-01 (Howard Hinnant's `civil_from_days`).
pub(crate) fn civil_day(days: u64) -> String {
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Inverse of `civil_day`.
pub(crate) fn day_number(day: &str) -> Option<u64> {
    let mut parts = day.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    u64::try_from(era * 146_097 + doe - 719_468).ok()
}
