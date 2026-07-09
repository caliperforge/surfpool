use std::collections::HashMap;

use once_cell::sync::Lazy;

#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub address: String,
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub logo_uri: Option<String>,
}

/// Normalize a CSV field after `parse_csv_line` has already handled quoting.
fn parse_csv_field(field: &str) -> String {
    field.trim().to_string()
}

/// Parse a CSV line into fields, handling quoted strings with commas
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    // Escaped quote
                    current.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                fields.push(parse_csv_field(&current));
                current.clear();
            }
            _ => current.push(c),
        }
    }
    fields.push(parse_csv_field(&current));
    fields
}

pub static VERIFIED_TOKENS_BY_SYMBOL: Lazy<HashMap<String, TokenInfo>> = Lazy::new(|| {
    let csv = include_str!("verified_tokens.csv");
    let mut map = HashMap::new();

    for (i, line) in csv.lines().enumerate() {
        if i == 0 {
            continue; // Skip header
        }
        if line.trim().is_empty() {
            continue;
        }

        let fields = parse_csv_line(line);
        if fields.len() < 5 {
            continue; // Skip malformed lines
        }

        // CSV columns: id,name,symbol,icon,decimals,...
        let address = fields[0].clone();
        let name = fields[1].clone();
        let symbol = fields[2].clone();
        let icon = fields[3].clone();
        let decimals: u8 = fields[4].parse().unwrap_or(0);

        let token = TokenInfo {
            address,
            name,
            symbol: symbol.clone(),
            decimals,
            logo_uri: if icon.is_empty() { None } else { Some(icon) },
        };

        map.insert(symbol.to_uppercase(), token);
    }

    map
});

#[cfg(test)]
mod tests {
    use super::parse_csv_line;

    #[test]
    fn parse_csv_line_handles_quoted_fields() {
        let fields = parse_csv_line(r#"address,"Token, Inc.",TKN,,9"#);

        assert_eq!(fields, ["address", "Token, Inc.", "TKN", "", "9"]);
    }

    #[test]
    fn parse_csv_line_preserves_literal_edge_quotes() {
        let fields = parse_csv_line(r#"address,"""quoted""",QT,,6"#);

        assert_eq!(fields[1], r#""quoted""#);
    }
}
