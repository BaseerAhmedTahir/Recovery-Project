//! `rc sqlite` - recover deleted rows from a SQLite database and its `-wal`.
//!
//! The database is read as bytes and never opened through SQLite, so no `-shm`
//! or journal appears next to it and the WAL is not checkpointed.

use crate::output::print_json;
use clap::Args as ClapArgs;
use rc_sqlite_carve::format::Value;
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct Args {
    /// The database file. A `-wal` beside it is read too.
    db: PathBuf,

    /// Only rows of this table.
    #[arg(long)]
    table: Option<String>,

    /// Maximum rows to print. 0 means no limit.
    #[arg(long, default_value_t = 100)]
    limit: usize,
}

fn show(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(t) => {
            let t: String = t.chars().take(80).collect();
            format!("{t:?}")
        }
        Value::Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}

pub fn run(args: Args, json: bool) -> anyhow::Result<()> {
    let mut report = rc_sqlite_carve::carve_file(&args.db)?;
    if let Some(t) = &args.table {
        report.recovered.retain(|r| &r.table == t);
    }
    if json {
        return print_json(&report);
    }
    println!(
        "{}: {} pages of {} bytes, WAL frames {} committed / {} uncommitted",
        args.db.display(),
        report.pages,
        report.page_size,
        report.wal_frames_committed,
        report.wal_frames_uncommitted
    );
    for t in &report.tables {
        let n = report
            .recovered
            .iter()
            .filter(|r| r.table == t.name)
            .count();
        println!(
            "  table {:<24} {:>7} live rows, {:>6} deleted rows recovered",
            t.name, t.live_rows, n
        );
    }
    for note in &report.notes {
        println!("note: {note}");
    }
    let limit = if args.limit == 0 {
        usize::MAX
    } else {
        args.limit
    };
    for r in report.recovered.iter().take(limit) {
        let cols = report
            .tables
            .iter()
            .find(|t| t.name == r.table)
            .map(|t| t.columns.as_slice())
            .unwrap_or(&[]);
        println!(
            "\n{} rowid={} from {:?} ({} +{})",
            r.table,
            r.rowid.map_or("?".into(), |i| i.to_string()),
            r.source,
            r.file,
            r.offset
        );
        for (c, v) in cols.iter().zip(&r.values) {
            if !matches!(v, Value::Null) {
                println!("    {:<20} {}", c.name, show(v));
            }
        }
    }
    Ok(())
}
