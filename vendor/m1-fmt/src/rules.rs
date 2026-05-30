use m1_core::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceDecision {
    None,
    Single,
}

/// Space before a token of kind `next`, given the previous token kind `prev`
/// and the containing node kind `parent`.
pub fn space_before(next: Kind, prev: Kind, _parent: Kind) -> SpaceDecision {
    use Kind::*;
    use SpaceDecision::*;

    match next {
        // No space before punctuation.
        Semicolon | Comma | Dot => None,

        // No space before closing paren/brace (printer handles newline+indent for }).
        RParen | RBrace => None,

        // No space before ( when previous token is an identifier (function call).
        LParen if matches!(prev, Identifier) => None,

        // Space before ( when it follows a control keyword.
        LParen if matches!(prev, If | When | Is | Expand) => Single,

        // No space after opening paren, or after a member-access dot.
        _ if matches!(prev, LParen | Dot) => None,

        // Binary operators: space before.
        Plus | Minus | Star | Slash | Percent | Assign | PlusEq | MinusEq | StarEq | SlashEq
        | EqEq | BangEq | Eq | Neq | Lt | Gt | LtEq | GtEq | AmpAmp | PipePipe | And | Or | Amp
        | Pipe | Caret | LtLt | GtGt | Question | Colon | To => Single,

        // Space after binary operators.
        _ if matches!(
            prev,
            Plus | Minus
                | Star
                | Slash
                | Percent
                | Assign
                | PlusEq
                | MinusEq
                | StarEq
                | SlashEq
                | EqEq
                | BangEq
                | Eq
                | Neq
                | Lt
                | Gt
                | LtEq
                | GtEq
                | AmpAmp
                | PipePipe
                | And
                | Or
                | Amp
                | Pipe
                | Caret
                | LtLt
                | GtGt
                | Question
                | Colon
                | To
        ) =>
        {
            Single
        }

        // Space after comma.
        _ if matches!(prev, Comma) => Single,

        // Space before opening brace.
        LBrace => Single,

        // Space after keywords when not followed by (.
        _ if matches!(prev, Local | Static) => Single,

        // Default: single space between tokens at the same level.
        _ => Single,
    }
}

/// Returns true if `kind` is a binary operator.
pub fn is_binary_op(kind: Kind) -> bool {
    use Kind::*;
    matches!(
        kind,
        Plus | Minus
            | Star
            | Slash
            | Percent
            | Assign
            | PlusEq
            | MinusEq
            | StarEq
            | SlashEq
            | EqEq
            | BangEq
            | Eq
            | Neq
            | Lt
            | Gt
            | LtEq
            | GtEq
            | AmpAmp
            | PipePipe
            | And
            | Or
            | Amp
            | Pipe
            | Caret
            | LtLt
            | GtGt
            | Question
            | Colon
            | To
    )
}

/// Returns true if `kind` is a control keyword that takes a `(` argument.
pub fn is_control_keyword(kind: Kind) -> bool {
    use Kind::*;
    matches!(kind, If | Else | When | Is | Expand)
}

/// Returns true if a space is needed before `(` given the previous token.
pub fn space_before_lparen(prev: Kind) -> SpaceDecision {
    if is_control_keyword(prev) {
        SpaceDecision::Single
    } else {
        SpaceDecision::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use m1_core::Kind;

    #[test]
    fn binary_op_gets_spaces() {
        assert!(is_binary_op(Kind::Plus));
        assert!(is_binary_op(Kind::Assign));
        assert!(is_binary_op(Kind::EqEq));
    }

    #[test]
    fn no_space_before_semicolon() {
        assert_eq!(
            space_before(Kind::Semicolon, Kind::Identifier, Kind::AssignmentStatement),
            SpaceDecision::None
        );
    }

    #[test]
    fn no_space_fn_call_paren() {
        assert_eq!(space_before_lparen(Kind::Identifier), SpaceDecision::None);
    }

    #[test]
    fn space_before_if_paren() {
        assert_eq!(space_before_lparen(Kind::If), SpaceDecision::Single);
    }
}
