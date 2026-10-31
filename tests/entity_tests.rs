use runki::deck::clean_html;

#[test]
fn decodes_entities_after_bare_ampersands() {
    assert_eq!(clean_html("A & B &amp; C"), "A & B & C");
}

#[test]
fn preserves_ampersand_with_semicolon_in_text() {
    assert_eq!(clean_html("R&D; done"), "R&D; done");
}

#[test]
fn decodes_uppercase_hex_entity_prefix() {
    assert_eq!(clean_html("&#X26;"), "&");
}

#[test]
fn preserves_unknown_well_formed_entity() {
    assert_eq!(clean_html("&foo;"), "&foo;");
}

#[test]
fn preserves_lone_trailing_ampersand() {
    assert_eq!(clean_html("trailing &"), "trailing &");
}

#[test]
fn decodes_supported_entities() {
    assert_eq!(clean_html("&lt;"), "<");
    assert_eq!(clean_html("&#38;"), "&");
    assert_eq!(clean_html("&#x26;"), "&");
    assert_eq!(clean_html("a&nbsp;b"), "a b");
}
