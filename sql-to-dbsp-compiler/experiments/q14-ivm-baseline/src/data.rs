//! The harness's rows: the three base tables, a transaction's change, the rows
//! of `V`, and the in-memory reference that the tests compare `V` with.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

/// A base-table row as the circuits' JSON inputs take it.
pub trait Row {
    /// The whole row.
    fn json(&self) -> Value;
    /// The primary key alone, which a deletion from an input map needs.
    fn key_json(&self) -> Value;
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct Part {
    pub key: i64,
    pub part_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct Partsupp {
    pub part_key: i64,
    pub supplier_key: i64,
    pub available: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct Line {
    pub order_key: i64,
    pub number: i32,
    pub part_key: i64,
    pub supplier_key: i64,
    pub price_cents: i64,
    pub discount_hundredths: i64,
    pub ship_date: String,
}

impl Row for Part {
    fn json(&self) -> Value {
        json!({"p_partkey": self.key, "p_type": self.part_type})
    }

    fn key_json(&self) -> Value {
        json!({"p_partkey": self.key})
    }
}

impl Part {
    pub fn parse(row: &Value) -> Result<Self> {
        Ok(Self {
            key: integer(row, "p_partkey")?,
            part_type: string(row, "p_type")?,
        })
    }
}

impl Row for Partsupp {
    fn json(&self) -> Value {
        json!({"ps_partkey": self.part_key, "ps_suppkey": self.supplier_key,
            "ps_availqty": self.available})
    }

    fn key_json(&self) -> Value {
        json!({"ps_partkey": self.part_key, "ps_suppkey": self.supplier_key})
    }
}

impl Partsupp {
    pub fn parse(row: &Value) -> Result<Self> {
        Ok(Self {
            part_key: integer(row, "ps_partkey")?,
            supplier_key: integer(row, "ps_suppkey")?,
            available: i32::try_from(integer(row, "ps_availqty")?)?,
        })
    }
}

impl Row for Line {
    fn json(&self) -> Value {
        json!({"l_orderkey": self.order_key, "l_linenumber": self.number,
            "l_partkey": self.part_key, "l_suppkey": self.supplier_key,
            "l_extendedprice": decimal_string(self.price_cents),
            "l_discount": decimal_string(self.discount_hundredths),
            "l_shipdate": self.ship_date})
    }

    fn key_json(&self) -> Value {
        json!({"l_orderkey": self.order_key, "l_linenumber": self.number})
    }
}

impl Line {
    pub fn parse(row: &Value) -> Result<Self> {
        Ok(Self {
            order_key: integer(row, "l_orderkey")?,
            number: i32::try_from(integer(row, "l_linenumber")?)?,
            part_key: integer(row, "l_partkey")?,
            supplier_key: integer(row, "l_suppkey")?,
            price_cents: hundredths(row, "l_extendedprice")?,
            discount_hundredths: hundredths(row, "l_discount")?,
            ship_date: string(row, "l_shipdate")?,
        })
    }
}

/// One transaction's change, or a net delta: rows with `true` for an insertion
/// and `false` for a deletion. Inserting a row whose primary key exists
/// replaces it.
#[derive(Clone, Debug, Default)]
pub struct Change {
    pub parts: Vec<(Part, bool)>,
    pub partsupps: Vec<(Partsupp, bool)>,
    pub lines: Vec<(Line, bool)>,
}

impl Change {
    pub fn len(&self) -> usize {
        self.parts.len() + self.partsupps.len() + self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The change as a transaction's steps: with `stepped`, three (parts,
    /// then Partsupp rows, then line items); otherwise one.
    pub fn steps(self, stepped: bool) -> Vec<Change> {
        if !stepped {
            return vec![self];
        }
        vec![
            Change {
                parts: self.parts,
                ..Change::default()
            },
            Change {
                partsupps: self.partsupps,
                ..Change::default()
            },
            Change {
                lines: self.lines,
                ..Change::default()
            },
        ]
    }
}

/// A row of `V`.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct ViewRow {
    pub part_key: i64,
    pub part_type: String,
    pub order_key: i64,
    pub line_number: i32,
    pub price_cents: i64,
    pub discount_hundredths: i64,
    pub ship_date: String,
}

impl ViewRow {
    pub fn new(part: &Part, line: &Line) -> Self {
        Self {
            part_key: part.key,
            part_type: part.part_type.clone(),
            order_key: line.order_key,
            line_number: line.number,
            price_cents: line.price_cents,
            discount_hundredths: line.discount_hundredths,
            ship_date: line.ship_date.clone(),
        }
    }

    pub fn parse(row: &Value) -> Result<Self> {
        Ok(Self {
            part_key: integer(row, "p_partkey")?,
            part_type: string(row, "p_type")?,
            order_key: integer(row, "l_orderkey")?,
            line_number: i32::try_from(integer(row, "l_linenumber")?)?,
            price_cents: hundredths(row, "l_extendedprice")?,
            discount_hundredths: hundredths(row, "l_discount")?,
            ship_date: string(row, "l_shipdate")?,
        })
    }
}

/// The base tables by primary key, the tests' in-memory oracle.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Reference {
    pub parts: BTreeMap<i64, Part>,
    pub partsupps: BTreeMap<(i64, i64), Partsupp>,
    pub lines: BTreeMap<(i64, i32), Line>,
}

impl Reference {
    pub fn apply(&mut self, change: &Change) {
        for (row, insert) in &change.parts {
            if *insert {
                self.parts.insert(row.key, row.clone());
            } else {
                self.parts.remove(&row.key);
            }
        }
        for (row, insert) in &change.partsupps {
            let key = (row.part_key, row.supplier_key);
            if *insert {
                self.partsupps.insert(key, row.clone());
            } else {
                self.partsupps.remove(&key);
            }
        }
        for (row, insert) in &change.lines {
            let key = (row.order_key, row.number);
            if *insert {
                self.lines.insert(key, row.clone());
            } else {
                self.lines.remove(&key);
            }
        }
    }

    /// `V`: each line item joined with its part, if the part exists.
    pub fn evaluate(&self) -> BTreeMap<ViewRow, i64> {
        let mut view = BTreeMap::new();
        for line in self.lines.values() {
            if let Some(part) = self.parts.get(&line.part_key) {
                *view.entry(ViewRow::new(part, line)).or_default() += 1;
            }
        }
        view
    }
}

/// `after - before`, without zeros.
pub fn difference<T: Ord + Clone>(
    before: &BTreeMap<T, i64>,
    after: &BTreeMap<T, i64>,
) -> BTreeMap<T, i64> {
    let mut delta = after.clone();
    for (row, weight) in before {
        *delta.entry(row.clone()).or_default() -= weight;
    }
    delta.retain(|_, weight| *weight != 0);
    delta
}

fn field<'a>(row: &'a Value, name: &str) -> Result<&'a Value> {
    row.as_object()
        .context("a row must be a JSON object")?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
        .with_context(|| format!("missing field {name}"))
}

fn integer(row: &Value, name: &str) -> Result<i64> {
    field(row, name)?
        .as_i64()
        .with_context(|| format!("{name} must be an integer"))
}

fn string(row: &Value, name: &str) -> Result<String> {
    field(row, name)?
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("{name} must be a string"))
}

/// A decimal field with at most two fractional digits, in hundredths. DBSP
/// writes decimals as JSON numbers.
fn hundredths(row: &Value, name: &str) -> Result<i64> {
    let value = field(row, name)?;
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    let (negative, unsigned) = text
        .strip_prefix('-')
        .map_or((false, text.as_str()), |rest| (true, rest));
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty()
        || fraction.len() > 2
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        bail!("{name} needs at most two fractional digits: {text}");
    }
    let magnitude = whole.parse::<i64>()? * 100 + format!("{fraction:0<2}").parse::<i64>()?;
    Ok(if negative { -magnitude } else { magnitude })
}

fn decimal_string(hundredths: i64) -> String {
    let sign = if hundredths < 0 { "-" } else { "" };
    let magnitude = hundredths.unsigned_abs();
    format!("{sign}{}.{:02}", magnitude / 100, magnitude % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_round_trip_through_their_json_text() {
        for cents in [0, 5, 100, 123_456, -7] {
            let row = json!({"d": decimal_string(cents)});
            assert_eq!(hundredths(&row, "d").unwrap(), cents);
        }
        assert_eq!(hundredths(&json!({"d": 1.5}), "d").unwrap(), 150);
        assert!(hundredths(&json!({"d": "1.234"}), "d").is_err());
        assert!(hundredths(&json!({"d": ".5"}), "d").is_err());
    }
}
