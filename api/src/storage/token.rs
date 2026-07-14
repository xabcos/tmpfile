use crate::config::{TOKEN_CHARSET, TOKEN_LENGTH};
use rand::RngExt;

pub struct TokenGenerator;

impl TokenGenerator {
    pub fn generate() -> String {
        let mut rng = rand::rng();
        let charset = TOKEN_CHARSET.as_bytes();
        let mut token = String::with_capacity(TOKEN_LENGTH);

        for _ in 0..TOKEN_LENGTH {
            let index = rng.random_range(0..charset.len());
            token.push(char::from(charset[index]));
        }

        token
    }

    pub fn parse_path(path: &str) -> Option<&str> {
        let token = path.get(..TOKEN_LENGTH)?;
        let suffix = path.get(TOKEN_LENGTH..)?;
        (token.bytes().all(is_token_byte) && (suffix.is_empty() || suffix.starts_with('.')))
            .then_some(token)
    }
}

fn is_token_byte(byte: u8) -> bool {
    TOKEN_CHARSET.as_bytes().contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_tokens_have_the_expected_entropy_and_alphabet() {
        let tokens: HashSet<_> = (0..1_000).map(|_| TokenGenerator::generate()).collect();

        assert_eq!(tokens.len(), 1_000);
        assert!(tokens.iter().all(|token| token.len() == TOKEN_LENGTH));
        assert!(
            tokens
                .iter()
                .flat_map(|token| token.bytes())
                .all(is_token_byte)
        );
    }

    #[test]
    fn parser_accepts_current_links_and_rejects_invalid_tokens() {
        let current = "0123456789abcdefghijklmno.png";
        assert_eq!(
            TokenGenerator::parse_path(current),
            Some("0123456789abcdefghijklmno")
        );

        assert_eq!(TokenGenerator::parse_path("invalid!.txt"), None);
        assert_eq!(TokenGenerator::parse_path("short"), None);
        assert_eq!(
            TokenGenerator::parse_path("0123456789abcdefghijklmnox"),
            None
        );
    }
}
