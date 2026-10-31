//! Usage ledger: the background writer into the shared database, replay and day arithmetic.

use std::sync::{Mutex as StdMutex, mpsc};

use super::{Inner, LedgerCommand};
use crate::store::Store;

pub(crate) fn ledger_worker(
    store: &Store,
    inner: &StdMutex<Inner>,
    receiver: mpsc::Receiver<LedgerCommand>,
) {
    for command in receiver {
        match command {
            LedgerCommand::Record(record) => {
                let written = serde_json::to_string(&record)
                    .map_err(anyhow::Error::from)
                    .and_then(|line| store.append_usage(record.ts, &line));
                let mut inner = inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                inner.ledger_pending -= 1;
                match written {
                    Ok(()) => inner.ledger_records += 1,
                    Err(error) => {
                        inner.ledger_failed += 1;
                        inner.ledger_error = Some(format!("{error:#}"));
                    }
                }
            }
            LedgerCommand::Flush(sender) => {
                let _ = sender.send(());
            }
        }
    }
}

/// Daily totals over every stored record, aggregated by the database.
/// Daily totals inside the report window: older rows stay on disk but are never shown, so startup
/// does not scan them.
pub(crate) fn replay(store: &Store) -> anyhow::Result<Vec<crate::store::DailyUsage>> {
    let since = (unix_day(super::now_seconds()) + 1)
        .saturating_sub(u64::from(super::REPORT_WINDOW_DAYS))
        * 86_400;
    store.usage_daily_totals(since)
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
