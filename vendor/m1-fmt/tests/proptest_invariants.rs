use proptest::prelude::*;

/// A token-extracting helper mirroring tests/semantic.rs.
fn tokens(src: &str) -> Vec<String> {
    use m1_core::{Kind, Node};
    fn go(node: Node, out: &mut Vec<String>) {
        if matches!(node.kind(), Kind::LineComment | Kind::BlockComment) {
            return;
        }
        let children = node.children();
        if children.is_empty() {
            let t = node.text().trim().to_string();
            if !t.is_empty() {
                out.push(t);
            }
        } else {
            for c in children {
                go(c, out);
            }
        }
    }
    let cst = m1_core::parse(src);
    let mut v = Vec::new();
    go(cst.root(), &mut v);
    v
}

/// Identifier-ish atoms; some long enough to force wrapping when combined.
fn atom() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("Short".to_string()),
        Just("A Medium Identifier Name".to_string()),
        Just("A Quite Long Identifier Name That Eats Many Columns".to_string()),
        (1u32..1000).prop_map(|n| n.to_string()),
    ]
}

/// A binary chain `a <op> b <op> c ...` of 1..6 atoms.
fn binary_chain() -> impl Strategy<Value = String> {
    let op = prop_oneof![Just("|"), Just("&&"), Just("+"), Just("=="), Just("&")];
    proptest::collection::vec((atom(), op), 1..6).prop_map(|parts| {
        let mut s = String::new();
        for (i, (a, o)) in parts.iter().enumerate() {
            if i == 0 {
                s.push_str(a);
            } else {
                s.push_str(&format!(" {} {}", o, a));
            }
        }
        s
    })
}

/// A call `Func(arg, arg, ...)`.
fn call() -> impl Strategy<Value = String> {
    proptest::collection::vec(atom(), 1..6)
        .prop_map(|args| format!("Module.Compute({})", args.join(", ")))
}

/// A whole statement.
fn statement() -> impl Strategy<Value = String> {
    prop_oneof![
        binary_chain().prop_map(|e| format!("Target Name = {};\n", e)),
        call().prop_map(|c| format!("Result = {};\n", c)),
        binary_chain().prop_map(|c| format!("if ({}) {{\n    Body = 1;\n}}\n", c)),
    ]
}

fn fragment() -> impl Strategy<Value = String> {
    proptest::collection::vec(statement(), 1..6).prop_map(|stmts| stmts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn idempotent(src in fragment()) {
        if !m1_core::parse(&src).syntax_diagnostics().is_empty() {
            return Ok(());
        }
        let once = m1_fmt::format_str(&src).unwrap().output;
        let twice = m1_fmt::format_str(&once).unwrap().output;
        prop_assert_eq!(once, twice);
    }

    #[test]
    fn reparses_clean(src in fragment()) {
        if !m1_core::parse(&src).syntax_diagnostics().is_empty() {
            return Ok(());
        }
        let out = m1_fmt::format_str(&src).unwrap().output;
        prop_assert!(m1_core::parse(&out).syntax_diagnostics().is_empty());
    }

    #[test]
    fn tokens_preserved(src in fragment()) {
        if !m1_core::parse(&src).syntax_diagnostics().is_empty() {
            return Ok(());
        }
        let out = m1_fmt::format_str(&src).unwrap().output;
        prop_assert_eq!(tokens(&src), tokens(&out));
    }
}
