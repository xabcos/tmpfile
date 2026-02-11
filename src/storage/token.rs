use crate::config::{TOKEN_CHARSET, TOKEN_LENGTH, TOKEN_TIME_FORMAT, TOKEN_TOTAL_LENGTH};
use chrono::Utc;
use rand::RngExt;
use std::fmt::Write as _;

pub struct TokenGenerator;

impl TokenGenerator {
    pub fn generate() -> String {
        let mut rng = rand::rng();
        let charset = TOKEN_CHARSET.as_bytes();
        let mut token = String::with_capacity(TOKEN_TOTAL_LENGTH);

        for _ in 0..TOKEN_LENGTH {
            let idx = rng.random_range(0..charset.len());
            token.push(char::from(charset[idx]));
        }

        let _ = write!(&mut token, "{}", Utc::now().format(TOKEN_TIME_FORMAT));
        token
    }
}
