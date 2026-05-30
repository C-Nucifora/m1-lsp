#[test]
fn format_str_with_default_matches_format_str() {
    let src = "x=1+2;\n";
    let a = m1_fmt::format_str(src).unwrap().output;
    let b = m1_fmt::format_str_with(src, &m1_fmt::FormatOptions::default())
        .unwrap()
        .output;
    assert_eq!(a, b);
}

#[test]
fn default_options_are_two_blank_lines_and_88() {
    let o = m1_fmt::FormatOptions::default();
    assert_eq!(o.max_blank_lines, 2);
    assert_eq!(o.line_width, 88);
}
