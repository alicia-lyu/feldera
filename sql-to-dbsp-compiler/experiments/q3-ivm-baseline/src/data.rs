use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

#[derive(Clone, Debug, Eq, PartialEq, size_of::SizeOf)]
pub struct Customer {
    pub key: i64,
    pub segment: String,
}

#[derive(Clone, Debug, Eq, PartialEq, size_of::SizeOf)]
pub struct Order {
    pub key: i64,
    pub customer_key: i64,
    pub date: String,
    pub ship_priority: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, size_of::SizeOf)]
pub struct Line {
    pub order_key: i64,
    pub number: i32,
    pub price_cents: i64,
    pub discount_hundredths: i64,
    pub ship_date: String,
}

fn fields(row: &str, count: usize) -> Result<Vec<&str>> {
    let mut parts: Vec<_> = row.split('|').collect();
    if parts.last() == Some(&"") {
        parts.pop();
    }
    if parts.len() != count {
        bail!("expected {count} dbgen fields, got {}", parts.len());
    }
    Ok(parts)
}

pub(crate) fn decimal_hundredths(value: &str, exact_two: bool) -> Result<i64> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |n| (true, n));
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || (exact_two && fraction.len() != 2)
        || fraction.len() > 2
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        bail!("decimal needs at most two fractional digits: {value}");
    }
    let major: i128 = whole.parse()?;
    let minor: i128 = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<i128>()? * 10,
        _ => fraction.parse()?,
    };
    let magnitude = major
        .checked_mul(100)
        .and_then(|n| n.checked_add(minor))
        .context("decimal overflow")?;
    let signed = if negative { -magnitude } else { magnitude };
    i64::try_from(signed).context("decimal overflow")
}

fn date(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        bail!("invalid dbgen date: {value}");
    }
    let year: i32 = value[..4].parse()?;
    let month: u32 = value[5..7].parse()?;
    let day: u32 = value[8..].parse()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&month) || day == 0 || day > days[(month - 1) as usize] {
        bail!("invalid dbgen date: {value}");
    }
    Ok(value.to_owned())
}

impl Customer {
    pub fn parse(row: &str) -> Result<Self> {
        let f = fields(row, 8)?;
        Ok(Self {
            key: f[0].parse()?,
            segment: f[6].to_owned(),
        })
    }

    pub fn json(&self) -> Value {
        json!({"c_custkey": self.key, "c_mktsegment": self.segment})
    }
}

impl Order {
    pub fn parse(row: &str) -> Result<Self> {
        let f = fields(row, 9)?;
        Ok(Self {
            key: f[0].parse()?,
            customer_key: f[1].parse()?,
            date: date(f[4])?,
            ship_priority: f[7].parse()?,
        })
    }

    pub fn json(&self) -> Value {
        json!({"o_orderkey": self.key, "o_custkey": self.customer_key,
            "o_orderdate": self.date, "o_shippriority": self.ship_priority})
    }
}

impl Line {
    pub fn parse(row: &str) -> Result<Self> {
        let f = fields(row, 16)?;
        Ok(Self {
            order_key: f[0].parse()?,
            number: f[3].parse()?,
            price_cents: decimal_hundredths(f[5], true)?,
            discount_hundredths: decimal_hundredths(f[6], true)?,
            ship_date: date(f[10])?,
        })
    }

    pub fn json(&self) -> Value {
        json!({"l_orderkey": self.order_key, "l_linenumber": self.number,
            "l_extendedprice": decimal_string(self.price_cents),
            "l_discount": decimal_string(self.discount_hundredths), "l_shipdate": self.ship_date})
    }
}

fn decimal_string(cents: i64) -> String {
    let magnitude = i128::from(cents).abs();
    format!(
        "{}{}.{:02}",
        if cents < 0 { "-" } else { "" },
        magnitude / 100,
        magnitude % 100
    )
}

pub fn delete_order_key(row: &str) -> Result<i64> {
    let value = row.strip_suffix('|').unwrap_or(row);
    Ok(value.parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbgen_fields_are_exact() {
        let customer = Customer::parse("1|n|a|1|p|1.00|BUILDING|comment|").unwrap();
        assert_eq!(customer.segment, "BUILDING");
        let order = Order::parse("1|2|O|1.00|1996-02-29|5-LOW|Clerk|0|c|").unwrap();
        assert_eq!(order.date, "1996-02-29");
        let line =
            Line::parse("1|2|3|4|5|21168.23|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|i|m|c|")
                .unwrap();
        assert_eq!((line.price_cents, line.discount_hundredths), (2_116_823, 4));
        assert_eq!(delete_order_key("7|").unwrap(), 7);
        assert!(Line::parse("1|2|").is_err());
        assert!(decimal_hundredths("0.041", true).is_err());
        assert!(decimal_hundredths("92233720368547758.08", true).is_err());
        assert_eq!(decimal_hundredths("-0.04", true).unwrap(), -4);
        assert_eq!(decimal_string(-4), "-0.04");
        assert!(date("1995-02-29").is_err());
    }
}
