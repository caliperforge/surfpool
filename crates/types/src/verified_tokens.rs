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

fn parse_tsv_line(line: &str) -> Vec<&str> {
    line.split('\t').map(str::trim).collect()
}

/// Every catalog row in TSV order. Option lists must use this: the by-symbol
/// map collapses tokens sharing a symbol and silently drops their mints.
pub static VERIFIED_TOKENS: Lazy<Vec<TokenInfo>> = Lazy::new(|| {
    let tsv = include_str!("verified_tokens.tsv");
    let mut tokens = Vec::new();

    for (i, line) in tsv.lines().enumerate() {
        if i == 0 {
            continue; // Skip header
        }
        if line.trim().is_empty() {
            continue;
        }

        let fields = parse_tsv_line(line);
        if fields.len() < 5 {
            continue; // Skip malformed lines
        }

        let address = fields[0].to_string();
        let name = fields[1].to_string();
        let symbol = fields[2].to_string();
        let icon = fields[3].to_string();
        let decimals: u8 = fields[4].parse().unwrap_or(0);

        tokens.push(TokenInfo {
            address,
            name,
            symbol,
            decimals,
            logo_uri: if icon.is_empty() { None } else { Some(icon) },
        });
    }

    tokens
});

pub static VERIFIED_TOKENS_BY_SYMBOL: Lazy<HashMap<String, TokenInfo>> = Lazy::new(|| {
    VERIFIED_TOKENS
        .iter()
        .map(|token| (token.symbol.to_uppercase(), token.clone()))
        .collect()
});

#[cfg(test)]
mod tests {
    use super::{VERIFIED_TOKENS, parse_tsv_line};

    #[test]
    fn verified_tokens_loads_every_row() {
        let rows = include_str!("verified_tokens.tsv")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
            - 1;
        assert_eq!(VERIFIED_TOKENS.len(), rows);

        let wbtc = VERIFIED_TOKENS
            .iter()
            .find(|token| token.address == "5XZw2LKTyrfvfiskJ78AMpackRjPcyCif1WhUsPDuVqQ")
            .expect("WBTC should load");
        assert_eq!(wbtc.symbol, "WBTC");
        assert_eq!(wbtc.decimals, 8);
        assert_eq!(
            wbtc.logo_uri.as_deref(),
            Some(
                "https://j73gp4w27ucraqtyzsqa5th4fwldfvqgq576cnceekfactgk4fla.arweave.net/T_Zn8tr9BRBCeMygDsz8LZYy1gaHf-E0RCKKAUzK4VY"
            )
        );
    }

    #[test]
    fn verified_tokens_lines_have_fourteen_fields() {
        for (i, line) in include_str!("verified_tokens.tsv").lines().enumerate() {
            assert_eq!(parse_tsv_line(line).len(), 14, "line {}", i + 1);
        }
    }

    #[test]
    fn parse_tsv_line_keeps_commas_in_fields() {
        let fields = parse_tsv_line("address\tToken, Inc.\tTKN\t\t9");

        assert_eq!(fields, ["address", "Token, Inc.", "TKN", "", "9"]);
    }
}
