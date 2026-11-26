//! Native monthly `PARTITION BY RANGE` partitions, without `pg_partman`.
//!
//! The month arithmetic is pure and tested everywhere. The DDL is built only
//! from integers this module computes, so nothing a client sent reaches SQL.
//! There is no `DEFAULT` partition: a row outside every partition is a
//! structured error (`no_partition`).

use sqlx::PgPool;

/// A calendar month in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Month {
    pub year: i32,
    pub month: u32,
}

/// The partitioned tables and their range key kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    WorkBuckets,
    ActivityRollups,
    Anchors,
}

impl Table {
    pub const ALL: [Table; 3] = [Table::WorkBuckets, Table::ActivityRollups, Table::Anchors];

    fn name(self) -> &'static str {
        match self {
            Table::WorkBuckets => "work_buckets",
            Table::ActivityRollups => "activity_rollups",
            Table::Anchors => "anchors",
        }
    }
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        i32::try_from(year).unwrap_or(i32::MAX),
        u32::try_from(month).unwrap_or(1),
    )
}

/// A civil date to days since 1970-01-01.
fn days_from_civil(year: i32, month: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl Month {
    /// The month containing `ms` (Unix milliseconds).
    pub fn of_ms(ms: u64) -> Month {
        let days = i64::try_from(ms / 86_400_000).unwrap_or(i64::MAX / 2);
        let (year, month) = civil_from_days(days);
        Month { year, month }
    }

    pub fn start_ms(self) -> i64 {
        days_from_civil(self.year, self.month) * 86_400_000
    }

    pub fn next(self) -> Month {
        self.plus(1)
    }

    pub fn plus(self, months: i32) -> Month {
        let index = self.year * 12 + i32::try_from(self.month).unwrap_or(1) - 1 + months;
        Month {
            year: index.div_euclid(12),
            month: u32::try_from(index.rem_euclid(12)).unwrap_or(0) + 1,
        }
    }

    fn suffix(self) -> String {
        format!("{:04}_{:02}", self.year, self.month)
    }

    fn timestamp_literal(self) -> String {
        format!("{:04}-{:02}-01 00:00:00+00", self.year, self.month)
    }
}

/// The months kept ready: the previous, the current and the next two.
pub fn standard_window(now_ms: u64) -> [Month; 4] {
    let current = Month::of_ms(now_ms);
    [current.plus(-1), current, current.plus(1), current.plus(2)]
}

/// The `CREATE TABLE ... PARTITION OF` statement for one table and month.
pub fn create_statement(table: Table, month: Month) -> String {
    let name = table.name();
    let next = month.next();
    let (from, to) = match table {
        Table::Anchors => (
            format!("'{}'", month.timestamp_literal()),
            format!("'{}'", next.timestamp_literal()),
        ),
        _ => (month.start_ms().to_string(), next.start_ms().to_string()),
    };
    format!(
        "CREATE TABLE IF NOT EXISTS {name}_{} PARTITION OF {name} FOR VALUES FROM ({from}) TO ({to})",
        month.suffix()
    )
}

/// Creates the partitions of `months` for every partitioned table. Safe to
/// call concurrently: an advisory lock serializes the DDL.
pub async fn ensure_months(pool: &PgPool, months: &[Month]) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7461)")
        .execute(&mut *tx)
        .await?;
    for month in months {
        for table in Table::ALL {
            sqlx::query(sqlx::AssertSqlSafe(create_statement(table, *month)))
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-26T12:00:00Z
    const NOV_26_2026: u64 = 1_795_694_400_000;

    #[test]
    fn the_month_of_a_timestamp() {
        assert_eq!(
            Month::of_ms(0),
            Month {
                year: 1970,
                month: 1
            }
        );
        assert_eq!(
            Month::of_ms(NOV_26_2026),
            Month {
                year: 2026,
                month: 11
            }
        );
        // The last millisecond of a month and the first of the next.
        let dec = Month {
            year: 2026,
            month: 12,
        };
        assert_eq!(
            Month::of_ms(u64::try_from(dec.start_ms()).unwrap() - 1).month,
            11
        );
        assert_eq!(Month::of_ms(u64::try_from(dec.start_ms()).unwrap()), dec);
    }

    #[test]
    fn months_roll_over_the_year() {
        let nov = Month {
            year: 2026,
            month: 11,
        };
        assert_eq!(
            nov.plus(2),
            Month {
                year: 2027,
                month: 1
            }
        );
        assert_eq!(
            nov.plus(-11),
            Month {
                year: 2025,
                month: 12
            }
        );
        assert_eq!(
            nov.plus(-1),
            Month {
                year: 2026,
                month: 10
            }
        );
    }

    #[test]
    fn the_standard_window_is_previous_current_and_next_two() {
        let months = standard_window(NOV_26_2026);
        assert_eq!(
            months.map(|m| (m.year, m.month)),
            [(2026, 10), (2026, 11), (2026, 12), (2027, 1)]
        );
    }

    #[test]
    fn known_epoch_boundaries() {
        // 2026-11-01T00:00:00Z and 2026-12-01T00:00:00Z.
        let nov = Month {
            year: 2026,
            month: 11,
        };
        assert_eq!(nov.start_ms(), 1_793_491_200_000);
        assert_eq!(nov.next().start_ms(), 1_796_083_200_000);
    }

    #[test]
    fn the_ddl_has_no_default_partition_and_only_computed_literals() {
        let nov = Month {
            year: 2026,
            month: 11,
        };
        assert_eq!(
            create_statement(Table::WorkBuckets, nov),
            "CREATE TABLE IF NOT EXISTS work_buckets_2026_11 PARTITION OF work_buckets \
             FOR VALUES FROM (1793491200000) TO (1796083200000)"
        );
        let anchors = create_statement(Table::Anchors, nov);
        assert!(anchors.contains("FROM ('2026-11-01 00:00:00+00') TO ('2026-12-01 00:00:00+00')"));
        assert!(!anchors.to_ascii_uppercase().contains("DEFAULT"));
    }
}
