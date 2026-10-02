//! The bytes a prompt crosses a PTY as, shared by the TUI's follow-up box
//! and the daemon's `nebula send`, so both submit a turn the same way.

/// Bracketed-paste markers around a pasted block, so the child (claude,
/// vim…) takes it as one paste rather than typing to auto-indent.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// `text` wrapped in the bracketed-paste markers, ready for a PTY.
pub fn bracketed(text: &str) -> Vec<u8> {
    let mut data = PASTE_START.to_vec();
    data.extend_from_slice(text.as_bytes());
    data.extend_from_slice(PASTE_END);
    data
}

/// `text` as an agent's next turn: two PTY writes, the prompt and then the
/// `\r` that submits it, so the child has the prompt in hand before the
/// Enter arrives. The prompt is a BRACKETED PASTE only when it has line
/// breaks — the CLI then takes it as one block instead of auto-indenting
/// it — and plain bytes when it is one line, which keeps it out of the
/// "[Pasted text]" placeholder those CLIs fold a paste into.
pub fn turn_writes(text: &str) -> [Vec<u8>; 2] {
    let prompt = if text.contains('\n') {
        bracketed(text)
    } else {
        text.as_bytes().to_vec()
    };
    [prompt, b"\r".to_vec()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_line_turn_is_its_bytes_then_a_carriage_return() {
        assert_eq!(turn_writes("next"), [b"next".to_vec(), b"\r".to_vec()]);
    }

    #[test]
    fn a_multi_line_turn_is_one_bracketed_paste_then_a_carriage_return() {
        assert_eq!(
            turn_writes("a\nb"),
            [b"\x1b[200~a\nb\x1b[201~".to_vec(), b"\r".to_vec()]
        );
    }
}
