use crate::compare::{self, Finding, Kind};

#[test]
fn duplicate_trailing_comment_stays_with_edited_function() {
    let old = "fn existing() { first(); } // keep\n";
    let new = concat!(
        "fn inserted() { first(); } // keep\n",
        "fn existing() { second(); } // keep\n",
    );
    assert_eq!(finding_lines(old, new, Kind::Comment), [1]);
}

#[test]
fn surrounding_code_disambiguates_identical_trailing_lines() {
    let old = "fn existing() {\n    first(); // keep\n}\n";
    let new = concat!(
        "fn inserted() {\n    first(); // keep\n}\n",
        "fn existing() {\n    first(1); // keep\n}\n",
    );
    assert_eq!(finding_lines(old, new, Kind::Comment), [2]);
}

#[test]
fn preserves_moved_comments_and_trailing_comments_next_to_edited_code() {
    let old = "// moved\nfn first() { call(1); } // retained\nfn second() {}\n";
    let new = "fn first() { call(2); } // retained\nfn second() {}\n// moved\n";
    assert!(compare::added_findings(old, new).is_empty());
}

#[test]
fn reports_a_rewritten_nested_block_at_its_unchanged_opening_line() {
    let old = "fn first() {}\n/* opening\n   /* nested */ old body\n*/\n";
    let new = "fn first() {}\n/* opening\n   /* nested */ new body\n*/\n";
    assert_eq!(finding_lines(old, new, Kind::Comment), [2]);
}

#[test]
fn normalizes_crlf_without_changing_locations() {
    let old = "/* opening\n   body */\nfn first() {} // keep\n";
    let mut crlf = old.replace('\n', "\r\n");
    assert!(compare::added_findings(old, &crlf).is_empty());
    crlf.push_str("// TODO\r\n");
    assert_eq!(finding_lines(old, &crlf, Kind::Comment), [4]);
}

#[test]
fn ignores_doc_comments_and_markers_in_literals() {
    let source = r##"//! inner line docs
/*! inner block docs */
/// outer line docs
/** outer block docs */
fn literals() {
    let _ = "escaped \" // not a comment";
    let _ = r#"// /* raw string
*/"#;
    let _ = b"// byte string";
    let _ = br"/* raw byte string */";
    let _ = c"// C string";
    let _ = cr#"/* raw C string */"#;
    let _ = ('/', b'/');
}
//// ordinary line comment
/*** ordinary block comment */
// fn disabled() {}
"##;
    assert_eq!(finding_lines("", source, Kind::Comment), [15, 16, 17]);
}

#[test]
fn finds_both_lint_groups_in_inner_and_outer_attributes() {
    let source = concat!(
        "#![allow(clippy::all)]\n",
        "#![allow(warnings)]\n",
        "#[allow(warnings)] fn first() {}\n",
        "#[allow(clippy::all)] fn second() {}\n",
        "#[allow(warnings, clippy::all)] fn third() {}\n",
    );
    assert_eq!(
        finding_lines("", source, Kind::BlanketAllow),
        [1, 2, 3, 4, 5]
    );
}

#[test]
fn warns_when_an_existing_allow_adds_a_blanket_lint_group() {
    let old = "#[allow(warnings)]\nfn kept() {}\n";
    let new = "#[allow(warnings, clippy::all)]\nfn kept() {}\n";
    assert_eq!(finding_lines(old, new, Kind::BlanketAllow), [1]);
}

#[test]
fn retains_existing_suppression_when_another_is_inserted_above_it() {
    let old = "#[allow(warnings)] fn kept() {}\n";
    let new = "#[allow(warnings)] fn inserted() {}\n#[allow(warnings, dead_code)]\nfn kept() {}\n";
    assert_eq!(finding_lines(old, new, Kind::BlanketAllow), [1]);
}

#[test]
fn ignores_targeted_allows_expect_attributes_and_attribute_like_text() {
    let source = r##"#![allow(dead_code)]
#![expect(warnings)]
// #[allow(warnings)]
/* #![allow(clippy::all)] */
#[allow(clippy::unused_self, reason = "warnings, clippy::all")]
#[expect(clippy::all)]
fn example() {
    let _ = "#![allow(warnings)]";
    let _ = r#"#[allow(clippy::all)]"#;
}
"##;
    assert_eq!(
        compare::added_findings("", source),
        [
            Finding {
                line: 3,
                kind: Kind::Comment
            },
            Finding {
                line: 4,
                kind: Kind::Comment
            }
        ]
    );
}

fn finding_lines(old: &str, new: &str, kind: Kind) -> Vec<usize> {
    compare::added_findings(old, new)
        .into_iter()
        .filter(|finding| finding.kind == kind)
        .map(|finding| finding.line)
        .collect()
}
