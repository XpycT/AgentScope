use agentscope::ingest::text::{display_safe, preview, redact, sanitize};

#[test]
fn strips_csi_osc_bidi_and_controls() {
    let input = "\x1b[31mred\x1b[0m \x1b]0;title\x07 \u{202e}reversed\x1b[2J";
    let out = sanitize(input);
    assert!(!out.contains('\u{1b}'));
    assert!(!out.contains('\u{202e}'));
    assert!(out.contains("red"));
    assert!(!out.contains("title"));
    assert!(out.contains("reversed"));
    assert!(!out.contains("[31m") && !out.contains("[2J"));
}

#[test]
fn strips_osc52_and_osc8_with_both_terminators() {
    assert_eq!(sanitize("a\x1b]52;c;SGVsbG8=\x07b"), "ab");
    assert_eq!(sanitize("a\x1b]52;c;SGVsbG8=\x1b\\b"), "ab");
    assert_eq!(
        sanitize("\x1b]8;;http://evil\x1b\\link\x1b]8;;\x1b\\"),
        "link"
    );
    assert_eq!(sanitize("a\u{9d}0;t\u{9c}b"), "ab");
}

#[test]
fn unterminated_osc_does_not_swallow_following_lines() {
    assert_eq!(sanitize("a\x1b]0;oops\nnext"), "a\nnext");
}

#[test]
fn strips_c0_c1_controls_but_keeps_newlines() {
    assert_eq!(
        sanitize("a\x00b\x07c\x08d\x7fe\u{85}f\u{9b}31mg\nh"),
        "abcdefg\nh"
    );
    assert_eq!(sanitize("x\x1bcy\x1b(Bz"), "xyz");
}

#[test]
fn removes_bidi_and_zero_width_directionals() {
    let input = "a\u{200b}b\u{200e}c\u{200f}d\u{202a}e\u{202d}f\u{2066}g\u{2069}h\u{feff}i\u{061c}j\u{2060}k";
    assert_eq!(sanitize(input), "abcdefghijk");
}

#[test]
fn expands_tabs_and_normalizes_line_endings() {
    assert_eq!(sanitize("a\tb"), "a   b");
    assert_eq!(sanitize("abcd\te"), "abcd    e");
    assert_eq!(sanitize("one\r\ntwo\rthree\n"), "one\ntwo\nthree\n");
}

#[test]
fn preserves_replacement_marker_and_wide_text() {
    let lossy = String::from_utf8_lossy(b"ok\xff\xfeend").into_owned();
    assert_eq!(sanitize(&lossy), "ok\u{fffd}\u{fffd}end");
    assert_eq!(sanitize("日本語 é"), "日本語 é");
}

#[test]
fn masks_common_secret_shapes_but_not_ordinary_code() {
    let out = redact("key sk-live-synthetic token=abc password = hunter2");
    assert!(!out.contains("sk-live-synthetic"));
    assert!(!out.contains("hunter2"));
    assert!(!out.contains("=abc"));
    assert!(redact("let token = compute()").contains("compute()"));
}

#[test]
fn masks_each_named_credential_shape() {
    let cases = [
        "sk-ant-api03-ABCDEFGH12345678",
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        "github_pat_11ABCDEFG0abcdefghij_klmnopqrstuv",
        "xoxb-1234567890-abcdefghij",
        "AKIAABCDEFGHIJKLMNOP",
        "Bearer abc123.def456-ghi",
        "bearer abcdefgh12345678",
        "api_key: \"s3cr3t value\"",
        "{\"password\": \"hunter2\"}",
        "MY_SECRET='top'",
        "auth_token=zzz999",
    ];
    for c in cases {
        let out = redact(&format!("x {c} y"));
        assert!(out.contains("[REDACTED]"), "not masked: {c} -> {out}");
        for secret in [
            "sk-ant-api03",
            "ghp_abc",
            "github_pat_11",
            "xoxb-1234",
            "AKIAABCD",
            "abc123.def",
            "abcdefgh12345678",
            "s3cr3t",
            "hunter2",
            "top",
            "zzz999",
        ] {
            assert!(!out.contains(secret), "leaked {secret}: {out}");
        }
    }
}

#[test]
fn masks_private_key_blocks_including_unterminated() {
    let full = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJB\nabc\n-----END RSA PRIVATE KEY-----\nafter";
    let out = redact(full);
    assert!(!out.contains("MIIBOg"));
    assert!(out.contains("before") && out.contains("after"));
    let cut = redact("-----BEGIN PRIVATE KEY-----\nMIIBOgIBAAJB");
    assert!(!cut.contains("MIIBOg"));
}

#[test]
fn does_not_mask_ordinary_text_and_code() {
    for ok in [
        "let token = compute()",
        "max_tokens: 4096",
        "if token == other { }",
        "the bearer token is stored elsewhere",
        "sk-1 and task-force-alpha",
        "skill-based routing",
        "tokenizer.encode(text)",
        "password = prompt_user()",
        "AKIA short",
        "ghp_short",
    ] {
        assert_eq!(redact(ok), ok);
    }
}

#[test]
fn escape_sequences_cannot_split_a_secret_from_redaction() {
    let out = display_safe("sk-\u{200b}live-synthetic\x1b[0m and sk-li\x1b]0;t\x07ve-synthetic");
    assert!(!out.contains("live-synthetic"), "{out}");
    assert!(!out.contains("ve-synthetic"), "{out}");
}

#[test]
fn redaction_is_idempotent() {
    let once = display_safe("password=hunter2 sk-live-synthetic");
    assert_eq!(display_safe(&once), once);
}

#[test]
fn preview_without_truncation_reports_original_length() {
    let p = preview("a\x1b[31mb\x1b[0m", 100, 100);
    assert_eq!(p.preview, "ab");
    assert!(!p.truncated);
    assert_eq!(p.byte_len, "a\x1b[31mb\x1b[0m".len());
    assert!(p.locator.is_none());
}

#[test]
fn preview_truncates_by_bytes_on_char_boundaries() {
    let p = preview("éééé", 5, 100);
    assert_eq!(p.preview, "éé");
    assert!(p.truncated);
    assert_eq!(p.byte_len, 8);
}

#[test]
fn preview_truncates_by_display_columns() {
    let p = preview("日本語テキスト", 1000, 6);
    assert_eq!(p.preview, "日本語");
    assert!(p.truncated);
    let p = preview("日本語", 1000, 5);
    assert_eq!(p.preview, "日本");
    let p = preview("abc", 1000, 3);
    assert_eq!(p.preview, "abc");
    assert!(!p.truncated);
}

#[test]
fn preview_never_exposes_partial_secret() {
    let p = preview("password=hunter2 tail", 14, 100);
    assert!(!p.preview.contains("hunter"));
    assert!(p.preview.starts_with("password=[RED"));
}

#[test]
fn preview_debug_is_content_free() {
    let p = preview("SENTINEL-CONTENT-XYZ", 100, 100);
    let dbg = format!("{p:?}");
    assert!(!dbg.contains("SENTINEL"), "{dbg}");
}

#[test]
fn escaped_quotes_do_not_end_a_quoted_credential() {
    let out = redact(r#"{"password":"ab\"SECRET_SUFFIX"} {'token': 'x\'TAIL'}"#);
    assert!(!out.contains("SECRET_SUFFIX"), "{out}");
    assert!(!out.contains("TAIL"), "{out}");
}

#[test]
fn exempt_call_does_not_swallow_later_assignments() {
    let out = redact(r#"token=compute();password="hunter2""#);
    assert!(out.contains("compute()"), "{out}");
    assert!(!out.contains("hunter2"), "{out}");
    let out = redact("token=compute() password=hunter2");
    assert!(!out.contains("hunter2"), "{out}");
}

#[test]
fn preview_uses_sequence_aware_width() {
    // U+2764 (width 1) + VS16 renders as width 2.
    let p = preview("\u{2764}\u{fe0f}x", 100, 1);
    assert!(!p.preview.contains('\u{fe0f}'), "{:?}", p.preview);
    assert!(p.truncated);
    let p = preview("\u{2764}\u{fe0f}x", 100, 2);
    assert_eq!(p.preview, "\u{2764}\u{fe0f}");
    let p = preview("ab\ncd", 100, 3);
    assert_eq!(p.preview, "ab\n");
}

#[test]
fn masks_whitespace_separated_credential_arguments() {
    for text in [
        "run --password synthetic-private-value now",
        "run --api-key synthetic-private-value now",
        "run --api_key 'synthetic private value' now",
        "run -token synthetic-private-value now",
    ] {
        let out = redact(text);
        assert!(!out.contains("synthetic"), "{out}");
        assert!(out.contains("now"), "{out}");
    }
    assert_eq!(redact("--password --verbose"), "--password --verbose");
    assert_eq!(redact("the password policy"), "the password policy");
}

#[test]
fn credential_arguments_with_separators_are_still_masked() {
    for text in [
        "--password = hunter2",
        "--api-key : value",
        "--password: hunter2",
    ] {
        let out = redact(text);
        assert!(!out.contains("hunter2") && !out.contains("value"), "{out}");
    }
}

#[test]
fn one_line_cells_never_start_a_new_screen_row() {
    use agentscope::ui::widgets::{fit, one_line};
    let hostile = "evil\x1b[31m title\u{202e}\nsecond\r\nline\x07 \x1b]0;owned\x07end";
    let cell = one_line(hostile);
    assert!(
        !cell.contains(['\n', '\r', '\x1b', '\x07', '\u{202e}']),
        "{cell:?}"
    );
    assert!(cell.contains("evil") && cell.contains("second") && cell.contains("end"));
    assert!(
        !cell.contains("owned"),
        "an OSC title payload is dropped, not shown"
    );
    // A fitted cell is exactly as wide as its column, whatever it held.
    assert_eq!(fit(&cell, 12).chars().count(), 12);
    assert_eq!(fit(&cell, 40).chars().count(), 40);
}
