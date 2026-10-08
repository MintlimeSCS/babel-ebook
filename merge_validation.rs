//! Dependency-free validation of the exact per-paragraph response contract.
pub(crate) fn validate_rows(
    rows: &[String],
    ids: &[String],
    sources: &[String],
) -> Result<Vec<String>, &'static str> {
    let fail = "Merged paragraph response failed identifier/count/content validation";
    if rows.len() != sources.len() || ids.len() != sources.len() {
        return Err(fail);
    }
    let mut output: Vec<String> = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let text = row.trim().strip_prefix(&ids[i]).ok_or(fail)?.trim();
        if text.is_empty()
            || !text.chars().any(char::is_alphabetic)
            || text.contains("[[BABEL")
            || text.contains('<')
            || text.contains('>')
        {
            return Err(fail);
        }
        if sources[i].chars().count() > 80
            && text.chars().count().saturating_mul(12) < sources[i].chars().count()
        {
            return Err(fail);
        }
        if output
            .iter()
            .enumerate()
            .any(|(j, previous)| previous == text && sources[j] != sources[i])
        {
            return Err(fail);
        }
        output.push(text.into());
    }
    Ok(output)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn check(rows: &[&str], sources: &[&str]) -> Result<Vec<String>, &'static str> {
        validate_rows(
            &rows.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &vec!["[[BABEL_P:t:0]]".into(), "[[BABEL_P:t:1]]".into()],
            &sources.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    }
    #[test]
    fn accepts_and_removes_identifiers_in_order() {
        assert_eq!(
            check(
                &["[[BABEL_P:t:0]]甲", "[[BABEL_P:t:1]]乙"],
                &["first", "second"]
            )
            .unwrap(),
            vec!["甲", "乙"]
        );
    }
    #[test]
    fn rejects_missing_and_extra_paragraphs() {
        assert!(check(&["[[BABEL_P:t:0]]甲"], &["first", "second"]).is_err());
        assert!(check(
            &["[[BABEL_P:t:0]]甲", "[[BABEL_P:t:1]]乙", "extra"],
            &["first", "second"]
        )
        .is_err());
    }
    #[test]
    fn rejects_reordered_identifiers() {
        assert!(check(
            &["[[BABEL_P:t:1]]乙", "[[BABEL_P:t:0]]甲"],
            &["first", "second"]
        )
        .is_err());
    }
    #[test]
    fn rejects_duplicate_identifier() {
        assert!(check(
            &["[[BABEL_P:t:0]]甲", "[[BABEL_P:t:0]]乙"],
            &["first", "second"]
        )
        .is_err());
    }
    #[test]
    fn rejects_empty_or_numeric_translation() {
        for value in ["", "123"] {
            assert!(check(
                &[&format!("[[BABEL_P:t:0]]{value}"), "[[BABEL_P:t:1]]乙"],
                &["first", "second"]
            )
            .is_err());
        }
    }
    #[test]
    fn rejects_injected_markup_and_leftover_ids() {
        for value in ["<a>甲</a>", "甲[[BABEL_P:t:1]]"] {
            assert!(check(
                &[&format!("[[BABEL_P:t:0]]{value}"), "[[BABEL_P:t:1]]乙"],
                &["first", "second"]
            )
            .is_err());
        }
    }
    #[test]
    fn rejects_duplicate_results_for_distinct_sources() {
        assert!(check(
            &["[[BABEL_P:t:0]]甲", "[[BABEL_P:t:1]]甲"],
            &["first", "second"]
        )
        .is_err());
        assert!(check(
            &["[[BABEL_P:t:0]]甲", "[[BABEL_P:t:1]]甲"],
            &["same", "same"]
        )
        .is_ok());
    }
    #[test]
    fn rejects_obviously_incomplete_long_paragraph() {
        let long = "English paragraph content ".repeat(15);
        assert!(check(
            &["[[BABEL_P:t:0]]甲", "[[BABEL_P:t:1]]乙"],
            &[&long, "second"]
        )
        .is_err());
    }
}
