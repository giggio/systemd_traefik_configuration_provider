pub fn sanitize_filename(s: &str) -> String {
    let ascii = deunicode::deunicode_with_tofu(s, "_");

    let mut out = String::with_capacity(ascii.len());

    for ch in ascii.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = regex::Regex::new(r"_+")
        .unwrap()
        .replace_all(out.trim_matches('_'), "_")
        .to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

pub trait AsyncMap {
    async fn async_map<F, Fut, T, U>(self, f: F) -> Vec<U>
    where
        F: Fn(T) -> Fut,
        Fut: Future<Output = U>,
        Self: Sized + IntoIterator<Item = T>,
    {
        futures::future::join_all(self.into_iter().map(f)).await
    }
}

impl<T, I> AsyncMap for I where I: IntoIterator<Item = T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_sanitize_filename_alphanumeric() {
        assert_eq!(sanitize_filename("myapp.service"), "myapp.service");
    }

    #[test]
    fn test_sanitize_filename_with_special_chars() {
        assert_eq!(sanitize_filename("my@app!service"), "my_app_service");
    }

    #[test]
    fn test_sanitize_filename_with_allowed_chars() {
        assert_eq!(
            sanitize_filename("my-app_service.tar"),
            "my-app_service.tar"
        );
    }

    #[test]
    fn test_sanitize_filename_only_special_chars() {
        assert_eq!(sanitize_filename("@#$%"), "untitled");
    }

    #[test]
    fn sanitize_empty_string() {
        let result = sanitize_filename("");
        assert_eq!(result, "untitled");
    }
}

#[cfg(all(test, feature = "proptests"))]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_sanitize_filename_never_panics(s in ".*") {
            let _ = sanitize_filename(&s);
        }

        #[test]
        fn prop_sanitize_filename_produces_valid_output(s in ".*") {
            let result = sanitize_filename(&s);
            prop_assert!(!result.contains('\0'), "Output contains null byte");
        }

        #[test]
        fn prop_sanitize_preserves_alphanumeric(s in "[a-zA-Z0-9]+") {
            let result = sanitize_filename(&s);
            prop_assert_eq!(result, s, "Alphanumeric characters should be preserved");
        }

        #[test]
        fn prop_sanitize_preserves_allowed_chars(s in "[a-zA-Z0-9.-]+") {
            let result = sanitize_filename(&s);
            prop_assert_eq!(result, s, "Allowed chars (., -) should be preserved");
        }

        #[test]
        fn prop_sanitize_is_idempotent(s in ".*") {
            let first = sanitize_filename(&s);
            let second = sanitize_filename(&first);
            prop_assert_eq!(first, second, "Sanitization should be idempotent");
        }

        #[test]
        fn prop_sanitize_output_is_safe(s in ".*") {
            let result = sanitize_filename(&s);
            for c in result.chars() {
                prop_assert!(
                    c.is_ascii_alphanumeric() || ".-_".contains(c),
                    "Output contains unsafe character: {}",
                    c
                );
            }
        }

        #[test]
        fn prop_sanitize_unicode(s in r"[áéó]{1,10}") {
            let result = sanitize_filename(&s);
            prop_assert_eq!(&s.replace("á", "a").replace("é", "e").replace("ó", "o"), &result);
            prop_assert_eq!(result.len(), s.chars().count());
        }

        #[test]
        fn prop_sanitize_only_allowed_special(s in "[.-]+") {
            let result = sanitize_filename(&s);
            prop_assert_eq!(result, s, "Allowed special chars should pass through");
        }

        #[test]
        fn prop_sanitize_spaces(s in "[ ]+") {
            let result = sanitize_filename(&s);
            prop_assert_eq!(result.clone(), "untitled", "Spaces should become 'untitled'");
        }

        #[test]
        fn prop_sanitize_mixed_content(
            alphanumeric in "[a-zA-Z0-9]{1,5}",
            special in "[@#$%!]"
        ) {
            let input = format!("{}{}", alphanumeric, special);
            let result = sanitize_filename(&input);
            prop_assert_eq!(result, format!("{}", alphanumeric));
        }
    }
}
