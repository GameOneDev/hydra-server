//! Reading query parameters the typed extractor can't express.

/// `shop` may be repeated (`?shop=steam&shop=launchbox`), which the typed
/// extractor collapses, so it is read off the raw query string.
pub fn shops_from_query(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else { return Vec::new() };

    raw.split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| *key == "shop")
        .map(|(_, value)| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_shop_the_launcher_repeats() {
        assert_eq!(
            shops_from_query(Some("shop=steam&shop=launchbox&take=10")),
            vec!["steam", "launchbox"]
        );
    }

    #[test]
    fn ignores_an_empty_or_missing_shop() {
        assert!(shops_from_query(None).is_empty());
        assert!(shops_from_query(Some("shop=&take=10")).is_empty());
    }
}
