/// `<sanitized title>.torrent`, falling back to the torrent's URL hash
/// (its last path segment) and finally "episode" when the title
/// sanitizes to nothing.
pub fn torrent_filename(title: &str, torrent_url: &str) -> String {
    let mut stem = sanitize_stem(title);
    if stem.is_empty() {
        stem = sanitize_stem(&url_stem(torrent_url));
    }
    if stem.is_empty() {
        stem = "episode".to_string();
    }
    format!("{stem}.torrent")
}

/// Characters unsafe to print to a terminal or embed in a filename: the
/// C0/C1 control codes `char::is_control` already covers, plus the Unicode
/// format and separator code points it misses — bidirectional overrides (the
/// Trojan-Source display-spoofing class), zero-width joiners/spaces, and the
/// line/paragraph separators some terminals render as newlines.
pub(crate) fn is_unsafe_display_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'   // ZWSP, ZWNJ, ZWJ, LRM, RLM
            | '\u{202A}'..='\u{202E}' // LRE, RLE, PDF, LRO, RLO
            | '\u{2060}'..='\u{2064}' // word joiner + invisible operators
            | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
            | '\u{2028}' | '\u{2029}' // line / paragraph separators
            | '\u{061C}'              // arabic letter mark
            | '\u{FEFF}'              // BOM / zero-width no-break space
        )
}

/// Drops every [`is_unsafe_display_char`] from `s`. Applied to
/// remote-controlled strings (feed titles, enclosure URLs) before they reach
/// the terminal or an exported file.
pub(crate) fn strip_unsafe(s: &str) -> String {
    s.chars().filter(|c| !is_unsafe_display_char(*c)).collect()
}

pub fn sanitize_stem(title: &str) -> String {
    let mapped: String = title
        .chars()
        .filter(|c| !is_unsafe_display_char(*c))
        .map(|c| match c {
            '/' => '⁄',
            '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect();
    mapped.trim().trim_end_matches(['.', ' ']).to_string()
}

fn url_stem(url: &str) -> String {
    let last = url
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_end_matches(".torrent");
    // Reject scheme-bearing fragments (e.g., "mailto:x") when the URL has no slashes
    if last.is_empty() || last.contains(':') {
        "".to_string()
    } else {
        last.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://mikanani.me/Download/20260628/b66771fd95710ab32a19a4797994196780c94527.torrent";

    #[test]
    fn keeps_cjk_brackets_and_fullwidth_colon() {
        assert_eq!(
            torrent_filename("[猎户压制部] Dr.STONE：Science Future [37] [1080p]", URL),
            "[猎户压制部] Dr.STONE：Science Future [37] [1080p].torrent"
        );
    }

    #[test]
    fn replaces_slash_with_fraction_slash() {
        assert_eq!(torrent_filename("新石纪 / Dr.STONE [37]", URL), "新石纪 ⁄ Dr.STONE [37].torrent");
    }

    #[test]
    fn replaces_ascii_windows_illegal_chars() {
        assert_eq!(
            torrent_filename(r#"a\b:c*d?e"f<g>h|i"#, URL),
            "a_b_c_d_e_f_g_h_i.torrent"
        );
    }

    #[test]
    fn strips_control_chars() {
        assert_eq!(torrent_filename("a\u{0}b\tc\u{7f}d", URL), "abcd.torrent");
    }

    #[test]
    fn trims_whitespace_and_trailing_dots() {
        assert_eq!(torrent_filename("  name.. ", URL), "name.torrent");
    }

    #[test]
    fn strips_bidi_and_zero_width_chars() {
        // RLO override + zero-width space + BOM must not survive into a filename.
        assert_eq!(
            torrent_filename("ab\u{202E}cd\u{200B}ef\u{feff}", URL),
            "abcdef.torrent"
        );
    }

    #[test]
    fn strip_unsafe_drops_format_and_separator_chars_but_keeps_cjk() {
        assert_eq!(strip_unsafe("a\u{202e}b\u{2028}c\u{feff}d"), "abcd");
        assert_eq!(strip_unsafe("石纪元 ： Dr.STONE"), "石纪元 ： Dr.STONE");
    }

    #[test]
    fn empty_title_falls_back_to_url_hash() {
        assert_eq!(
            torrent_filename("...", URL),
            "b66771fd95710ab32a19a4797994196780c94527.torrent"
        );
    }

    #[test]
    fn empty_title_and_useless_url_falls_back_to_episode() {
        assert_eq!(torrent_filename("...", "https://mikanani.me/"), "episode.torrent");
    }

    #[test]
    fn empty_title_with_url_having_illegal_chars_in_filename() {
        assert_eq!(torrent_filename("...", "https://x/file?x.torrent"), "file_x.torrent");
    }
}
