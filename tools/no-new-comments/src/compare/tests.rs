use crate::compare;

#[test]
fn duplicate_trailing_comment_stays_with_edited_function() {
    let old = "fn existing() { first(); } // keep\n";
    let new = concat!(
        "fn inserted() { first(); } // keep\n",
        "fn existing() { second(); } // keep\n",
    );
    assert_eq!(compare::added_comment_lines(old, new), [1]);
}

#[test]
fn surrounding_code_disambiguates_identical_trailing_lines() {
    let old = "fn existing() {\n    first(); // keep\n}\n";
    let new = concat!(
        "fn inserted() {\n    first(); // keep\n}\n",
        "fn existing() {\n    first(1); // keep\n}\n",
    );
    assert_eq!(compare::added_comment_lines(old, new), [2]);
}

#[test]
fn preserves_moved_comments_and_trailing_comments_next_to_edited_code() {
    let old = "// moved\nfn first() { call(1); } // retained\nfn second() {}\n";
    let new = "fn first() { call(2); } // retained\nfn second() {}\n// moved\n";
    assert!(compare::added_comment_lines(old, new).is_empty());
}

#[test]
fn reports_a_rewritten_nested_block_at_its_unchanged_opening_line() {
    let old = "fn first() {}\n/* opening\n   /* nested */ old body\n*/\n";
    let new = "fn first() {}\n/* opening\n   /* nested */ new body\n*/\n";
    assert_eq!(compare::added_comment_lines(old, new), [2]);
}

#[test]
fn normalizes_crlf_without_changing_locations() {
    let old = "/* opening\n   body */\nfn first() {} // keep\n";
    let mut crlf = old.replace('\n', "\r\n");
    assert!(compare::added_comment_lines(old, &crlf).is_empty());
    crlf.push_str("// TODO\r\n");
    assert_eq!(compare::added_comment_lines(old, &crlf), [4]);
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
    assert_eq!(compare::added_comment_lines("", source), [15, 16, 17]);
}
