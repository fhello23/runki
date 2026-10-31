use runki::deck::clean_html;

#[test]
fn non_cloze_curly_braces_are_left_alone() {
    assert_eq!(
        clean_html("use {{curly}} braces, a::b"),
        "use {{curly}} braces, a::b"
    );
}

#[test]
fn cloze_with_hint_reveals_answer() {
    assert_eq!(clean_html("{{c1::Paris::city}}"), "Paris");
}

#[test]
fn multi_digit_cloze_reveals_answer() {
    assert_eq!(clean_html("{{c12::x}}"), "x");
}

#[test]
fn two_clozes_in_one_field() {
    assert_eq!(
        clean_html("{{c1::Paris}} is in {{c2::France}}"),
        "Paris is in France"
    );
}

#[test]
fn cloze_without_digits_is_left_as_is() {
    assert_eq!(clean_html("{{c::x}}"), "{{c::x}}");
}

#[test]
fn non_cloze_before_real_cloze() {
    assert_eq!(clean_html("{{cat}} and {{c1::y}}"), "{{cat}} and y");
}
