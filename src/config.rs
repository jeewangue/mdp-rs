//! Runtime defaults, env-var overrides, and template placeholder resolution.

use anyhow::Result;
use std::path::Path;

pub struct BookConfig {
    pub title: String,
    pub author: String,
    pub plantuml_server: String,
    pub src_dir_display: String,
    pub language: String,
}

impl BookConfig {
    pub fn new(src_dir: &Path, title_override: Option<String>) -> Result<Self> {
        let title = reject_control_chars(
            &title_override.unwrap_or_else(|| {
                src_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Markdown Preview".into())
            }),
            "--title",
        )?;

        let author_raw = std::env::var("MDP_AUTHOR")
            .unwrap_or_else(|_| whoami().unwrap_or_else(|| "mdp".into()));
        let author = reject_control_chars(&author_raw, "$MDP_AUTHOR")?;

        let plantuml_raw = std::env::var("MDP_PLANTUML_SERVER")
            .unwrap_or_else(|_| "https://www.plantuml.com/plantuml".into());
        let plantuml_server = validate_plantuml_server(&plantuml_raw)?;

        let language = resolve_language()?;

        let src_dir_display = src_dir.display().to_string();

        Ok(Self { title, author, plantuml_server, src_dir_display, language })
    }
}

/// Resolve the BCP-47-ish book language. Priority:
///   1. `MDP_BOOK_LANG` (explicit, anything passing the validator)
///   2. `LANG` / `LC_ALL` env first segment (`ko_KR.UTF-8` → `ko`)
///   3. fallback to `"en"` — mdbook search-index needs a real value.
fn resolve_language() -> Result<String> {
    if let Ok(v) = std::env::var("MDP_BOOK_LANG") {
        return validate_language(&v);
    }
    let from_locale = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LANG"))
        .ok()
        .and_then(|v| {
            let head = v.split(['_', '.', '@']).next().unwrap_or("");
            (!head.is_empty() && !head.eq_ignore_ascii_case("c") && !head.eq_ignore_ascii_case("posix"))
                .then(|| head.to_lowercase())
        });
    Ok(from_locale.unwrap_or_else(|| "en".to_string()))
}

/// Validator: `[A-Za-z]{2,3}` (optionally `-region`). mdbook embeds `language`
/// directly in the rendered HTML `<html lang>`; rejecting anything fancy keeps
/// us safe from injection while still accepting all real-world tags.
fn validate_language(raw: &str) -> Result<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 12 {
        anyhow::bail!("$MDP_BOOK_LANG must be 1..12 chars, got {raw:?}");
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphabetic() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        anyhow::bail!(
            "$MDP_BOOK_LANG must be ASCII alpha/digit/-/_ (BCP-47-ish), got {raw:?}"
        );
    }
    Ok(s.to_lowercase())
}

fn whoami() -> Option<String> {
    std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok())
}

/// Reject control characters (NUL through 0x1F except regular space, plus DEL).
/// These break TOML strings and comments — easier to refuse at the boundary than
/// try to sanitize at every interpolation site.
fn reject_control_chars(s: &str, what: &str) -> Result<String> {
    if let Some(c) = s.chars().find(|c| c.is_control()) {
        anyhow::bail!(
            "{what} must not contain control characters (found {:?}); got {:?}",
            c,
            s,
        );
    }
    Ok(s.to_string())
}

/// PlantUML server must be an http(s) URL. mdbook-plantuml accepts either a URL
/// OR a local binary path — the latter would let a hostile env var silently
/// execute a local binary, so we require a URL.
fn validate_plantuml_server(raw: &str) -> Result<String> {
    if !(raw.starts_with("http://") || raw.starts_with("https://")) {
        anyhow::bail!(
            "$MDP_PLANTUML_SERVER must be an http(s) URL, got {raw:?}. \
             mdbook-plantuml also accepts local binary paths but mdp rejects them \
             to avoid accidental command execution."
        );
    }
    reject_control_chars(raw, "$MDP_PLANTUML_SERVER")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_newline_in_title() {
        let r = reject_control_chars("a\nb", "--title");
        assert!(r.is_err());
    }

    #[test]
    fn accepts_plain_title() {
        let r = reject_control_chars("My Notes", "--title").unwrap();
        assert_eq!(r, "My Notes");
    }

    #[test]
    fn accepts_korean_title() {
        let r = reject_control_chars("안녕 markdown", "--title").unwrap();
        assert_eq!(r, "안녕 markdown");
    }

    #[test]
    fn plantuml_requires_http() {
        assert!(validate_plantuml_server("/usr/bin/evil").is_err());
        assert!(validate_plantuml_server("evil").is_err());
        assert!(validate_plantuml_server("ftp://x").is_err());
        assert!(validate_plantuml_server("http://example.com").is_ok());
        assert!(validate_plantuml_server("https://www.plantuml.com/plantuml").is_ok());
    }

    #[test]
    fn plantuml_rejects_control_chars_even_with_http_prefix() {
        // A URL that starts with http but contains a newline — the scheme check
        // alone isn't enough.
        assert!(validate_plantuml_server("https://ok\nmalicious").is_err());
        assert!(validate_plantuml_server("http://ok\tevil").is_err());
    }

    #[test]
    fn reject_control_chars_catches_all_below_0x20() {
        for c in 0u32..=0x1f {
            let s = String::from(char::from_u32(c).unwrap());
            // 0x20 is the space char and should pass; everything below is a
            // control char and must be rejected.
            if c == 0x00 && s == " " {
                continue;
            }
            assert!(
                reject_control_chars(&s, "x").is_err(),
                "{c:#x} should have been rejected"
            );
        }
        // space and non-control chars pass
        assert!(reject_control_chars(" ", "x").is_ok());
        assert!(reject_control_chars("a b c", "x").is_ok());
        assert!(reject_control_chars("한글", "x").is_ok());
    }

    #[test]
    fn reject_control_chars_catches_embedded_control_in_long_string() {
        let s = format!("{}{}{}", "a".repeat(50), '\u{7}', "b".repeat(50));
        assert!(reject_control_chars(&s, "x").is_err());
    }

    #[test]
    fn plantuml_rejects_empty_and_unicode_only() {
        assert!(validate_plantuml_server("").is_err());
        assert!(validate_plantuml_server("한글://server").is_err());
    }

    #[test]
    fn validate_language_accepts_common_codes() {
        assert_eq!(validate_language("en").unwrap(), "en");
        assert_eq!(validate_language("EN").unwrap(), "en");
        assert_eq!(validate_language("ko").unwrap(), "ko");
        assert_eq!(validate_language("en-US").unwrap(), "en-us");
        assert_eq!(validate_language("zh_CN").unwrap(), "zh_cn");
    }

    #[test]
    fn validate_language_rejects_garbage() {
        assert!(validate_language("").is_err());
        assert!(validate_language(" ").is_err());
        assert!(validate_language("한글").is_err());
        assert!(validate_language("en;evil").is_err());
        assert!(validate_language("en\";injection=x").is_err());
        assert!(validate_language(&"a".repeat(20)).is_err());
    }
}
