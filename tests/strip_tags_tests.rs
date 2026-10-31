use runki::deck::clean_html;

#[test]
fn bare_less_than_is_preserved() {
    assert_eq!(clean_html("Is 3 < 5?"), "Is 3 < 5?");
    assert_eq!(clean_html("x < y and z > w"), "x < y and z > w");
}

#[test]
fn unterminated_tag_is_preserved() {
    assert_eq!(clean_html("hello <b"), "hello <b");
    assert_eq!(clean_html("a<b"), "a<b");
}

#[test]
fn real_tags_and_comments_still_stripped() {
    assert_eq!(clean_html("<b>Bold</b>"), "Bold");
    assert_eq!(clean_html("a<br>b"), "a\nb");
    assert_eq!(clean_html("a<br/>b"), "a\nb");
    assert_eq!(clean_html("<img src=\"x.png\">"), "[image]");
    assert_eq!(clean_html("<div>one</div><div>two</div>"), "one\ntwo");
    assert_eq!(clean_html("<!-- comment -->hello"), "hello");
    assert_eq!(clean_html("hello <!-- comment --> world"), "hello  world");
}
