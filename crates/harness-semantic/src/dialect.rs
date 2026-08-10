//! Per-engine SQL differences. Shapes SQL text only — never opens a connection.

/// What the compiler needs to know about an engine.
pub trait Dialect: Send + Sync {
    fn name(&self) -> &'static str;
    fn quote_ident(&self, id: &str) -> String;
    fn date_trunc(&self, grain: &str, expr: &str) -> String;

    /// Truncate to `grain` **in `tz`**, an IANA zone name.
    ///
    /// `None` means this engine cannot do it, and the compiler then refuses the
    /// query. There is deliberately **no default implementation**: a default
    /// returning `None` would let a newly added dialect lose timezone support
    /// without anybody writing a line about it, and a default that ignored `tz`
    /// would bucket in the session zone while the model says otherwise — the
    /// silent version of the bug this method exists to fix.
    fn date_trunc_tz(&self, grain: &str, expr: &str, tz: &str) -> Option<String>;

    /// Bind placeholder for the i-th argument (1-based).
    fn placeholder(&self, i: usize) -> String;
    /// Null-safe equality, for joining CTEs on dimensions that may be NULL.
    fn distinct_from(&self, l: &str, r: &str) -> String;

    /// Makes an expression divide as a decimal rather than as an integer.
    ///
    /// `SUM()` over an integer column returns an integer on most engines, and
    /// integer ÷ integer truncates: a defect rate of 1.47% comes back as 0.
    /// The query runs clean and returns a number, which is the worst way for a
    /// metric to be wrong. Engines disagree on how to say it — SQLite's DECIMAL
    /// is NUMERIC affinity and still divides as an integer, so it needs REAL.
    fn cast_decimal(&self, expr: &str) -> String;
}

/// The ANSI spelling most engines accept. 28 integer digits is more than any
/// real measure needs, and keeping the scale exact matters: a float would make
/// two runs of the same reconciliation disagree in the last place, and a control
/// query that fails intermittently gets switched off.
fn cast_decimal38(e: &str) -> String {
    format!("CAST({e} AS DECIMAL(38,10))")
}

fn dq(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}
fn bt(id: &str) -> String {
    format!("`{}`", id.replace('`', "``"))
}

pub struct Postgres;
impl Dialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }
    fn quote_ident(&self, id: &str) -> String {
        dq(id)
    }
    fn date_trunc(&self, g: &str, e: &str) -> String {
        format!("date_trunc('{g}', {e})")
    }
    /// `timestamptz AT TIME ZONE 'x'` yields the local wall clock as a plain
    /// `timestamp`, which is exactly what a bucket label should be.
    fn date_trunc_tz(&self, g: &str, e: &str, tz: &str) -> Option<String> {
        Some(format!("date_trunc('{g}', ({e}) AT TIME ZONE '{tz}')"))
    }
    fn placeholder(&self, i: usize) -> String {
        format!("${i}")
    }
    fn distinct_from(&self, l: &str, r: &str) -> String {
        format!("{l} IS NOT DISTINCT FROM {r}")
    }
    fn cast_decimal(&self, e: &str) -> String {
        format!("CAST({e} AS numeric)")
    }
}

/// Portable fallback: `?` placeholders, best-effort `date_trunc`.
pub struct Ansi;
impl Dialect for Ansi {
    fn name(&self) -> &'static str {
        "ansi"
    }
    fn quote_ident(&self, id: &str) -> String {
        dq(id)
    }
    fn date_trunc(&self, g: &str, e: &str) -> String {
        format!("date_trunc('{g}', {e})")
    }
    /// Refused. There is no portable spelling — `AT TIME ZONE`,
    /// `CONVERT_TIMEZONE` and `from_utc_timestamp` are three different engines'
    /// answers, and picking one here would produce a syntax error on the other
    /// two while claiming to be the portable dialect.
    fn date_trunc_tz(&self, _: &str, _: &str, _: &str) -> Option<String> {
        None
    }
    fn placeholder(&self, _: usize) -> String {
        "?".into()
    }
    fn distinct_from(&self, l: &str, r: &str) -> String {
        format!("({l} = {r} OR ({l} IS NULL AND {r} IS NULL))")
    }
    fn cast_decimal(&self, e: &str) -> String {
        cast_decimal38(e)
    }
}

pub struct Snowflake;
impl Dialect for Snowflake {
    fn name(&self) -> &'static str {
        "snowflake"
    }
    fn quote_ident(&self, id: &str) -> String {
        dq(id)
    }
    fn date_trunc(&self, g: &str, e: &str) -> String {
        format!("DATE_TRUNC('{g}', {e})")
    }
    fn date_trunc_tz(&self, g: &str, e: &str, tz: &str) -> Option<String> {
        Some(format!(
            "DATE_TRUNC('{g}', CONVERT_TIMEZONE('{tz}', {e}))"
        ))
    }
    fn placeholder(&self, i: usize) -> String {
        format!(":{i}")
    }
    fn distinct_from(&self, l: &str, r: &str) -> String {
        format!("{l} IS NOT DISTINCT FROM {r}")
    }
    fn cast_decimal(&self, e: &str) -> String {
        format!("CAST({e} AS NUMBER(38,10))")
    }
}

pub struct Databricks;
impl Dialect for Databricks {
    fn name(&self) -> &'static str {
        "databricks"
    }
    fn quote_ident(&self, id: &str) -> String {
        bt(id)
    }
    fn date_trunc(&self, g: &str, e: &str) -> String {
        format!("date_trunc('{}', {e})", g.to_uppercase())
    }
    fn date_trunc_tz(&self, g: &str, e: &str, tz: &str) -> Option<String> {
        Some(format!(
            "date_trunc('{}', from_utc_timestamp({e}, '{tz}'))",
            g.to_uppercase()
        ))
    }
    fn placeholder(&self, _: usize) -> String {
        "?".into()
    }
    fn distinct_from(&self, l: &str, r: &str) -> String {
        format!("{l} <=> {r}")
    }
    fn cast_decimal(&self, e: &str) -> String {
        cast_decimal38(e)
    }
}

pub struct DuckDb;
impl Dialect for DuckDb {
    fn name(&self) -> &'static str {
        "duckdb"
    }
    fn quote_ident(&self, id: &str) -> String {
        dq(id)
    }
    fn date_trunc(&self, g: &str, e: &str) -> String {
        format!("date_trunc('{g}', {e})")
    }
    fn date_trunc_tz(&self, g: &str, e: &str, tz: &str) -> Option<String> {
        Some(format!("date_trunc('{g}', ({e}) AT TIME ZONE '{tz}')"))
    }
    fn placeholder(&self, i: usize) -> String {
        format!("${i}")
    }
    fn distinct_from(&self, l: &str, r: &str) -> String {
        format!("{l} IS NOT DISTINCT FROM {r}")
    }
    fn cast_decimal(&self, e: &str) -> String {
        cast_decimal38(e)
    }
}

/// MySQL and MariaDB.
///
/// Neither has `DATE_TRUNC` — not in 8.x, not in MariaDB — so each grain is
/// emulated with date arithmetic that returns a DATE. Returning a DATE rather
/// than a formatted string matters: a bucket label has to sort chronologically
/// and compare against date literals in a WHERE clause, and `'2024-10'` as text
/// does neither.
pub struct MySql;
impl Dialect for MySql {
    fn name(&self) -> &'static str {
        "mysql"
    }
    fn quote_ident(&self, id: &str) -> String {
        bt(id)
    }
    fn date_trunc(&self, g: &str, e: &str) -> String {
        match g.to_lowercase().as_str() {
            "day" => format!("DATE({e})"),
            // WEEKDAY() is 0 on Monday, so this lands on Monday — the same week
            // start Postgres uses. MySQL's own WEEK() defaults to Sunday, which
            // would silently shift every weekly bucket by a day.
            "week" => format!("DATE_SUB(DATE({e}), INTERVAL WEEKDAY({e}) DAY)"),
            "month" => format!("DATE_SUB(DATE({e}), INTERVAL DAYOFMONTH({e})-1 DAY)"),
            "quarter" => {
                format!("DATE_ADD(MAKEDATE(YEAR({e}), 1), INTERVAL QUARTER({e})-1 QUARTER)")
            }
            "year" => format!("MAKEDATE(YEAR({e}), 1)"),
            // An unrecognised grain must not silently become "close enough":
            // this is a syntax error naming the offending grain, which is loud,
            // rather than a plausible number bucketed the wrong way.
            _ => format!("DATE_TRUNC('{g}', {e})"),
        }
    }
    /// Converts first, then truncates with the same emulation.
    ///
    /// **`CONVERT_TZ` with a named zone needs the timezone tables loaded**
    /// (`mysql_tzinfo_to_sql`); without them it returns NULL. That failure is
    /// loud in the right way — every row lands in one NULL bucket, which nobody
    /// mistakes for a correct answer — rather than the quiet eight-hour shift
    /// this method exists to remove. Offsets (`+08:00`) work without the tables,
    /// but they do not know about DST, so a zone name is the right thing to
    /// declare and a NULL column is the right way to find out it is missing.
    fn date_trunc_tz(&self, g: &str, e: &str, tz: &str) -> Option<String> {
        Some(self.date_trunc(g, &format!("CONVERT_TZ({e}, '+00:00', '{tz}')")))
    }
    fn placeholder(&self, _: usize) -> String {
        "?".into()
    }
    fn distinct_from(&self, l: &str, r: &str) -> String {
        format!("{l} <=> {r}")
    }
    fn cast_decimal(&self, e: &str) -> String {
        cast_decimal38(e)
    }
}

/// SQLite: no `date_trunc` and no date type — dates are text, and the modifiers
/// on `date()` are the only truncation available. Buckets come back as
/// `'YYYY-MM-DD'`, the one text date format that sorts chronologically.
pub struct Sqlite;
impl Dialect for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }
    fn quote_ident(&self, id: &str) -> String {
        dq(id)
    }
    fn date_trunc(&self, g: &str, e: &str) -> String {
        match g.to_lowercase().as_str() {
            "day" => format!("date({e})"),
            // strftime('%w') is 0 on Sunday; (%w + 6) % 7 makes Monday the 0
            // day, so subtracting it lands on Monday like Postgres.
            "week" => format!(
                "date({e}, '-' || ((CAST(strftime('%w', {e}) AS INTEGER) + 6) % 7) || ' days')"
            ),
            "month" => format!("date({e}, 'start of month')"),
            "quarter" => format!(
                "date({e}, 'start of year', '+' || (3 * ((CAST(strftime('%m', {e}) AS INTEGER) - 1) / 3)) || ' months')"
            ),
            "year" => format!("date({e}, 'start of year')"),
            _ => format!("DATE_TRUNC('{g}', {e})"),
        }
    }
    /// Refused. SQLite ships no timezone database — `date(x, 'localtime')` uses
    /// the *host's* zone, which makes the same query bucket differently on a
    /// developer's laptop and on the server. A fixed `'+8 hours'` would ignore
    /// DST. Neither is a timezone; both are a wrong answer that runs.
    fn date_trunc_tz(&self, _: &str, _: &str, _: &str) -> Option<String> {
        None
    }
    fn placeholder(&self, _: usize) -> String {
        "?".into()
    }
    fn distinct_from(&self, l: &str, r: &str) -> String {
        format!("{l} IS {r}")
    }
    fn cast_decimal(&self, e: &str) -> String {
        format!("CAST({e} AS REAL)")
    }
}

/// SQL Server (T-SQL).
///
/// `DATETRUNC` exists only from 2022, so truncation uses the DATEADD/DATEDIFF
/// idiom every supported version understands. Its epoch (0 = 1900-01-01) is a
/// Monday, so the week grain agrees with Postgres. `IS NOT DISTINCT FROM` is
/// likewise 2022-only, hence the explicit null pair.
pub struct SqlServer;
impl Dialect for SqlServer {
    fn name(&self) -> &'static str {
        "sqlserver"
    }
    fn quote_ident(&self, id: &str) -> String {
        format!("[{}]", id.replace(']', "]]"))
    }
    fn date_trunc(&self, g: &str, e: &str) -> String {
        let g = g.to_lowercase();
        match g.as_str() {
            "day" | "week" | "month" | "quarter" | "year" => {
                format!("DATEADD({g}, DATEDIFF({g}, 0, {e}), 0)")
            }
            _ => format!("DATETRUNC({g}, {e})"),
        }
    }
    /// Refused. T-SQL's `AT TIME ZONE` takes **Windows** zone names
    /// (`China Standard Time`), not IANA ones (`Asia/Shanghai`), and the model
    /// declares IANA. Translating between the two needs a mapping table that
    /// would be wrong for exactly the zones nobody tests. A refusal names the
    /// problem; a guessed mapping ships a query that buckets by the wrong offset.
    fn date_trunc_tz(&self, _: &str, _: &str, _: &str) -> Option<String> {
        None
    }
    fn placeholder(&self, i: usize) -> String {
        format!("@p{i}")
    }
    fn distinct_from(&self, l: &str, r: &str) -> String {
        format!("({l} = {r} OR ({l} IS NULL AND {r} IS NULL))")
    }
    fn cast_decimal(&self, e: &str) -> String {
        cast_decimal38(e)
    }
}

/// Resolves a dialect by name (case-insensitive). `None` for an unknown name,
/// so callers fail loudly instead of guessing.
pub fn by_name(name: &str) -> Option<Box<dyn Dialect>> {
    match name.to_lowercase().as_str() {
        "postgres" | "postgresql" | "pg" | "" => Some(Box::new(Postgres)),
        "snowflake" => Some(Box::new(Snowflake)),
        "databricks" | "spark" => Some(Box::new(Databricks)),
        "duckdb" => Some(Box::new(DuckDb)),
        "mysql" | "mariadb" => Some(Box::new(MySql)),
        "sqlite" | "sqlite3" => Some(Box::new(Sqlite)),
        "sqlserver" | "mssql" | "tsql" => Some(Box::new(SqlServer)),
        "ansi" => Some(Box::new(Ansi)),
        _ => None,
    }
}
