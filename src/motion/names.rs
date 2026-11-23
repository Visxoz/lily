//! Names as a match compares them, shared by Apple's and Spotify's lookups:
//! a wrong clip is worse than none, so only names that surely agree match.

use icu_normalizer::DecomposingNormalizerBorrowed;

/// Lowercase, without accents, without Apple's " - Single" or " - EP", and
/// only letters and digits.
pub fn normalised(name: &str) -> String {
    let mut name = name;
    for suffix in [" - Single", " - EP"] {
        name = name.strip_suffix(suffix).unwrap_or(name);
    }
    // Decomposed, an accent is a mark of its own, which the filter drops.
    DecomposingNormalizerBorrowed::new_nfd()
        .normalize(name)
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The whole credit and each name it joins, never part of one, as "Low" in
/// "Flowers".
pub fn credits(name: &str) -> Vec<String> {
    std::iter::once(normalised(name))
        .chain(name.split([',', '&']).map(normalised))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_accents_and_symbols() {
        assert_eq!(normalised("Beyoncé"), "beyonce");
        assert_eq!(normalised("AC/DC"), "acdc");
        assert_eq!(normalised("Midnights - Single"), "midnights");
        assert_eq!(normalised("Red - EP"), "red");
        assert_eq!(normalised("÷"), "");
    }

    #[test]
    fn credits_split_only_at_joins() {
        let credits = credits("Simon & Garfunkel, Low");
        assert!(credits.contains(&"simongarfunkellow".to_owned()));
        assert!(credits.contains(&"simon".to_owned()));
        assert!(credits.contains(&"garfunkel".to_owned()));
        assert!(credits.contains(&"low".to_owned()));
        assert!(!super::credits("Flowers").contains(&"low".to_owned()));
    }
}
