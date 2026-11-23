//! The driver's inputs: dbgen's base files, loaded in transactions of a
//! bounded size, and the refreshes, read into memory before any of them runs.
//!
//! Q14's tables are Part, Partsupp, and Lineitem, so TPC-H's refreshes reach
//! it only through Lineitem: RF1 inserts the line items of `lineitem.tbl.uN`,
//! and RF2 deletes every line item of the orders that `delete.N` names.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::data::{delete_order_key, Change, Line, Part, Partsupp};

/// dbgen's base files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaseFiles {
    pub part: PathBuf,
    pub partsupp: PathBuf,
    pub lineitem: PathBuf,
}

/// Loads the base files in transactions of at most `batch` rows: the parts,
/// then the Partsupp rows, then the line items. MI_P needs no owner for a row,
/// so a transaction may cut anywhere. Returns the number of rows.
pub fn load(
    files: &BaseFiles,
    batch: usize,
    transact: impl FnMut(Change) -> Result<()>,
) -> Result<usize> {
    if batch == 0 {
        bail!("the load batch must be positive");
    }
    let mut loader = Loader {
        batch,
        pending: Change::default(),
        rows: 0,
        transact,
    };
    loader.file(&files.part, Part::from_tbl, |change| &mut change.parts)?;
    loader.file(&files.partsupp, Partsupp::from_tbl, |change| {
        &mut change.partsupps
    })?;
    loader.file(&files.lineitem, Line::from_tbl, |change| &mut change.lines)?;
    loader.flush()?;
    Ok(loader.rows)
}

struct Loader<F> {
    batch: usize,
    pending: Change,
    rows: usize,
    transact: F,
}

impl<F: FnMut(Change) -> Result<()>> Loader<F> {
    fn file<T>(
        &mut self,
        path: &Path,
        parse: impl Fn(&str) -> Result<T>,
        rows_of: impl Fn(&mut Change) -> &mut Vec<(T, bool)>,
    ) -> Result<()> {
        for_each_row(path, parse, |row| {
            rows_of(&mut self.pending).push((row, true));
            self.rows += 1;
            if self.pending.len() >= self.batch {
                self.flush()?;
            }
            Ok(())
        })
    }

    fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        (self.transact)(std::mem::take(&mut self.pending))
    }
}

/// Calls `visit` on each row of a dbgen file, in file order.
fn for_each_row<T>(
    path: &Path,
    parse: impl Fn(&str) -> Result<T>,
    mut visit: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    for (number, text) in BufReader::new(file).lines().enumerate() {
        let row = parse(&text?).with_context(|| format!("{}:{}", path.display(), number + 1))?;
        visit(row)?;
    }
    Ok(())
}

fn rows<T>(path: &Path, parse: impl Fn(&str) -> Result<T>) -> Result<Vec<T>> {
    let mut rows = Vec::new();
    for_each_row(path, parse, |row| {
        rows.push(row);
        Ok(())
    })?;
    Ok(rows)
}

/// One item of a refresh list, a JSON array in execution order. It is the
/// format of the Q3 harness's lists, so that one `prepare_inputs.py` output
/// serves both; a Q3 list that cuts refreshes into transactions is refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshItem {
    /// RF1's orders. Q14 has no Orders table, so the driver does not read it.
    #[serde(default, rename = "rf1_orders")]
    _rf1_orders: Option<PathBuf>,
    rf1_lines: Option<PathBuf>,
    rf2: Option<PathBuf>,
}

/// A refresh, read into memory so that running it reads no file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Refresh {
    /// RF1: the line items to insert.
    Rf1(Vec<Line>),
    /// RF2: the orders whose line items to delete. The line items are found
    /// when the refresh runs, in the base tables' state at that time.
    Rf2(BTreeSet<i64>),
}

impl Refresh {
    pub fn phase(&self) -> &'static str {
        match self {
            Self::Rf1(_) => "rf1",
            Self::Rf2(_) => "rf2",
        }
    }
}

/// Reads the refresh list at `path` and every file it names. Relative paths
/// resolve against the list's directory.
pub fn read_refreshes(path: &Path) -> Result<Vec<Refresh>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let items: Vec<RefreshItem> = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("reading the refresh list {}", path.display()))?;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    items
        .into_iter()
        .enumerate()
        .map(|(number, item)| match (item.rf1_lines, item.rf2) {
            (Some(lines), None) => Ok(Refresh::Rf1(rows(&directory.join(lines), Line::from_tbl)?)),
            (None, Some(keys)) => Ok(Refresh::Rf2(
                rows(&directory.join(keys), delete_order_key)?
                    .into_iter()
                    .collect(),
            )),
            _ => bail!("refresh {}: needs either rf1_lines or rf2", number + 1),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PART: &str = "1|n|m|b|TYPE A|1|c|1.00|x|\n2|n|m|b|TYPE B|1|c|1.00|x|\n";
    const PARTSUPP: &str = "1|10|5|1.00|x|\n2|10|5|1.00|x|\n";

    fn line(order: i64, number: i32, part: i64) -> String {
        format!("{order}|{part}|10|{number}|1|10.00|0.05|0.01|N|O|1996-01-02|d|d|i|m|c|\n")
    }

    fn write(directory: &Path, name: &str, text: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn base_files(directory: &Path) -> BaseFiles {
        let lines: String = (1..=3).map(|order| line(order, 1, order % 2 + 1)).collect();
        BaseFiles {
            part: write(directory, "part.tbl", PART),
            partsupp: write(directory, "partsupp.tbl", PARTSUPP),
            lineitem: write(directory, "lineitem.tbl", &lines),
        }
    }

    #[test]
    fn loading_cuts_every_file_into_bounded_transactions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut transactions = Vec::new();
        let rows = load(&base_files(directory.path()), 3, |change| {
            transactions.push(change);
            Ok(())
        })?;
        assert_eq!(rows, 7);
        assert_eq!(
            transactions.iter().map(Change::len).collect::<Vec<_>>(),
            [3, 3, 1]
        );
        // The second transaction holds the last Partsupp row and two line items.
        assert_eq!(transactions[1].partsupps.len(), 1);
        assert_eq!(transactions[1].lines.len(), 2);
        assert!(load(&base_files(directory.path()), 0, |_| Ok(())).is_err());
        Ok(())
    }

    #[test]
    fn loading_names_the_bad_row() {
        let directory = tempfile::tempdir().unwrap();
        let files = base_files(directory.path());
        write(
            directory.path(),
            "partsupp.tbl",
            "1|10|5|1.00|x|\n1|20|five|1.00|x|\n",
        );
        let error = load(&files, 10, |_| Ok(())).unwrap_err();
        assert!(format!("{error:#}").contains("partsupp.tbl:2"), "{error:#}");
    }

    #[test]
    fn a_refresh_list_reads_rf1_and_rf2_relative_to_itself() -> Result<()> {
        let directory = tempfile::tempdir()?;
        write(directory.path(), "lineitem.tbl.u1", &line(9, 1, 1));
        write(directory.path(), "delete.1", "1|\n3|\n1|\n");
        let list = write(
            directory.path(),
            "list.json",
            r#"[{"rf1_orders": "orders.tbl.u1", "rf1_lines": "lineitem.tbl.u1"},
                {"rf2": "delete.1"}]"#,
        );
        let refreshes = read_refreshes(&list)?;
        assert_eq!(refreshes.len(), 2);
        assert!(matches!(&refreshes[0], Refresh::Rf1(lines) if lines.len() == 1));
        assert_eq!(refreshes[1], Refresh::Rf2([1, 3].into()));
        Ok(())
    }

    #[test]
    fn a_refresh_list_refuses_what_q14_cannot_run() -> Result<()> {
        let directory = tempfile::tempdir()?;
        write(directory.path(), "delete.1", "1|\n");
        for (name, text) in [
            ("neither.json", r#"[{}]"#),
            (
                "both.json",
                r#"[{"rf1_lines": "delete.1", "rf2": "delete.1"}]"#,
            ),
            (
                "cut.json",
                r#"[{"rf2": "delete.1", "transaction_orders": [1]}]"#,
            ),
            ("missing.json", r#"[{"rf2": "delete.2"}]"#),
        ] {
            let list = write(directory.path(), name, text);
            assert!(read_refreshes(&list).is_err(), "{name}");
        }
        Ok(())
    }
}
