//! Terminal-capability response filtering at the PTY boundary
//! (client → PTY direction only).
//!
//! Adopted from OpenDray's `terminal_capabilities.go` (ADR §6). Browser
//! emulators answer capability queries on their own: xterm.js replies to Device
//! Attributes, Cursor Position Report and Device Status Report requests without
//! the user typing anything. Those replies arrive on the same channel as
//! keystrokes, and an Ink-based CLI reading them during startup mis-parses them
//! as input — the known Ink-CLI startup breakage. Filtering at this one
//! chokepoint protects every client emulator instead of asking each of them to
//! behave.
//!
//! Only the responses are stripped. Everything a human or a mouse can produce
//! passes through byte-for-byte, which is why the shapes we strip are limited
//! to forms a terminal only emits as an answer: `c` (DA), `n` (DSR), the
//! `?`-prefixed `u` final (kitty flag report — key events are digit-led, so
//! `?` cannot collide with them), the `*`-marked `{` final (macro space
//! report), the `OSC 10/11/12` colour reports and the DCS `!~` checksum report.
//!
//! **The rule: strip the reply shapes `CapabilityProxy` emits, and only
//! those.** A client answer to a query the proxy answers itself is a
//! duplicate; a client answer to a query the proxy passes through is the only
//! answer the app will get, so it must reach the child. `R` (CPR), `OSC 4`
//! (palette) and the DCS replies — `>|` (XTVERSION), `!|` (tertiary DA), `$r`
//! (DECRQSS) and `+r`/`+R` (XTGETTCAP) — are therefore left through. The one
//! approximation is the CSI `n` final: the proxy answers part of the DSR `?`
//! family and passes the rest, and the reply shapes overlap, so every `n` is
//! stripped. The reference client (SwiftTerm) does not answer the passed-through
//! DSR queries, so nothing is lost with it today.
//!
//! **Known cost: replayed queries are re-answered.** A query the proxy passes
//! through is also stored in the session's replay ring, so a client attaching
//! with replay sees it again and answers it again. Because the filter now lets
//! those answers through, the child receives that stale reply as input on each
//! such attach — after the app that asked may have exited, where a shell or an
//! Ink-based CLI reads it as typed bytes. CPR has always worked this way. The
//! real fix is to keep passed-through queries out of the ring copy while still
//! forwarding them live; until then, losing the only answer (the alternative)
//! is the worse failure.
//!
//! **Chunk-scoped by design.** The filter holds no cross-call state: an escape
//! sequence split across two WebSocket frames passes through unfiltered. The
//! alternative — carrying a partial sequence to the next chunk — would delay a
//! bare <kbd>Esc</kbd> keypress until the *next* keystroke, breaking vi-style
//! editors, and no timeout is available at this layer. Emulators write each
//! capability reply with a single send, so the split case is rare, and the cost
//! of missing it is the status quo rather than a new failure.

use std::borrow::Cow;

/// Upper bound on how far we scan for a sequence terminator. A longer run is
/// treated as not-a-sequence and passed through, so a malformed or hostile
/// stream cannot make the scanner walk an unbounded distance per byte.
const MAX_SEQUENCE_LEN: usize = 256;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Stateless, chunk-scoped capability-response filter with strip counters for
/// the abuse/observability metrics.
#[derive(Debug, Default)]
pub struct TermFilter {
    stripped_sequences: u64,
    stripped_bytes: u64,
}

impl TermFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of capability responses stripped so far.
    pub fn stripped_sequences(&self) -> u64 {
        self.stripped_sequences
    }

    /// Number of bytes removed so far.
    pub fn stripped_bytes(&self) -> u64 {
        self.stripped_bytes
    }

    /// Filter one client→PTY chunk. Returns the input untouched (borrowed, no
    /// allocation) when there is nothing to strip, which is the common case for
    /// ordinary typing.
    pub fn filter<'a>(&mut self, chunk: &'a [u8]) -> Cow<'a, [u8]> {
        if !chunk.contains(&ESC) {
            return Cow::Borrowed(chunk);
        }

        let mut out: Option<Vec<u8>> = None;
        let mut i = 0;
        let mut copied_upto = 0;
        while i < chunk.len() {
            if chunk[i] != ESC {
                i += 1;
                continue;
            }
            match classify(&chunk[i..]) {
                Some(len) => {
                    let out = out.get_or_insert_with(|| Vec::with_capacity(chunk.len()));
                    out.extend_from_slice(&chunk[copied_upto..i]);
                    self.stripped_sequences += 1;
                    self.stripped_bytes += len as u64;
                    i += len;
                    copied_upto = i;
                }
                // Not a capability response: skip the ESC and keep scanning from
                // the next byte, so an SS3 function key (`ESC O P`) or a bare
                // ESC keypress is untouched.
                None => i += 1,
            }
        }

        match out {
            Some(mut out) => {
                out.extend_from_slice(&chunk[copied_upto..]);
                Cow::Owned(out)
            }
            None => Cow::Borrowed(chunk),
        }
    }
}

/// If `s` starts with a terminal-capability *response*, return its length.
fn classify(s: &[u8]) -> Option<usize> {
    debug_assert_eq!(s.first(), Some(&ESC));
    match s.get(1)? {
        b'[' => classify_csi(s),
        b']' => classify_osc(s),
        b'P' => classify_dcs(s),
        _ => None,
    }
}

/// `CSI ... c` (Device Attributes) and `CSI ... n` (Device Status Report) are
/// responses answered at the source; `CSI ... R` (CPR) is left through so the
/// client can answer what the runtime cannot. Two more finals are stripped
/// only in their response-specific forms: `?`-led `u` (kitty flag report —
/// digit-led `u` is kitty keyboard input and passes) and `*`-marked `{`
/// (macro space report). Every other final byte belongs to input a terminal
/// legitimately sends: `A`–`D` arrows, `~` tilde keys and bracketed paste,
/// `M`/`m` mouse, `I`/`O` focus.
fn classify_csi(s: &[u8]) -> Option<usize> {
    let mut i = 2;
    while i < s.len() && i <= MAX_SEQUENCE_LEN {
        let b = s[i];
        match b {
            // Parameter bytes (including the `?`, `>`, `<`, `=` private
            // markers) and intermediates.
            0x20..=0x3f => i += 1,
            // Final byte.
            0x40..=0x7e => {
                // `R` (CPR) is intentionally NOT stripped: the runtime does
                // not answer CPR (no screen state), so the client's CPR reply
                // must be allowed through to the child. DA (`c`), DSR (`n`),
                // the kitty flag report (`?`-led `u`) and the macro space
                // report (`*`-marked `{`) are answered at the source by
                // CapabilityProxy — a client echo of them here is redundant,
                // so they stay filtered.
                return if matches!(b, b'c' | b'n')
                    || (b == b'u' && s.get(2) == Some(&b'?'))
                    || (b == b'{' && s[2..i].contains(&b'*'))
                {
                    Some(i + 1)
                } else {
                    None
                };
            }
            // Anything else (control byte, 8-bit) means this is not a
            // well-formed CSI sequence; leave it alone.
            _ => return None,
        }
    }
    None
}

/// OSC color reports: `OSC Ps ; rgb:<...> ST|BEL`, e.g. the `OSC 10`/`OSC 11`
/// foreground/background answers and `OSC 12` cursor answers.
/// Deliberately narrow: only payloads carrying an `rgb:`/`rgba:` reply for
/// proxied colour queries (10, 11, 12) are stripped; `OSC 4` palette answers
/// pass through to the child, matching CPR.
fn classify_osc(s: &[u8]) -> Option<usize> {
    let (payload, len) = string_sequence(s, 2)?;
    let mut j = 0;
    // Ps ;  (one or more numeric groups)
    while j < payload.len() && payload[j].is_ascii_digit() {
        j += 1;
    }
    if j == 0 || payload.get(j) != Some(&b';') {
        return None;
    }
    let ps = &payload[..j];
    if matches!(ps, b"10" | b"11" | b"12")
        && (payload[j..].starts_with(b";rgb:") || payload[j..].starts_with(b";rgba:"))
    {
        Some(len)
    } else {
        None
    }
}

/// DCS report shapes: only the macro checksum report (`DCS <id> ! ~ <hex>`),
/// the one DCS reply CapabilityProxy emits. XTVERSION (`DCS > | ...`),
/// tertiary DA (`DCS ! | ...`), DECRQSS (`DCS 0$r` / `DCS 1$r`) and XTGETTCAP
/// (`DCS 0+r` / `DCS 1+r`) answers are left through: the proxy passes those
/// queries to the client, so the client's answer is the only one the child
/// gets. Other DCS payloads are passed through rather than assumed to be
/// replies.
fn classify_dcs(s: &[u8]) -> Option<usize> {
    let (payload, len) = string_sequence(s, 2)?;
    let mut j = 0;
    while j < payload.len() && payload[j].is_ascii_digit() {
        j += 1;
    }
    if payload[j..].starts_with(b"!~") {
        Some(len)
    } else {
        None
    }
}

/// Scan a string-terminated sequence (OSC/DCS) starting at `body` within `s`.
/// Returns the payload and the total sequence length including the terminator.
fn string_sequence(s: &[u8], body: usize) -> Option<(&[u8], usize)> {
    let mut i = body;
    // Same bound as the CSI scan (and the proxy's): the terminator is found
    // through index MAX_SEQUENCE_LEN, keeping the two directions consistent.
    while i < s.len() && i <= MAX_SEQUENCE_LEN {
        if s[i] == BEL {
            return Some((&s[body..i], i + 1));
        }
        if s[i] == ESC && s.get(i + 1) == Some(&b'\\') {
            return Some((&s[body..i], i + 2));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered(input: &[u8]) -> Vec<u8> {
        TermFilter::new().filter(input).to_vec()
    }

    #[test]
    fn strips_primary_and_secondary_device_attributes() {
        assert!(filtered(b"\x1b[?62;22c").is_empty());
        assert!(filtered(b"\x1b[>0;276;0c").is_empty());
        assert!(filtered(b"\x1b[?1;2c").is_empty());
    }

    #[test]
    fn passes_unproxied_replies_for_the_child_to_receive() {
        // CPR, OSC 4 palette and XTVERSION are not proxied, so the client's
        // answers must reach the child.
        assert_eq!(filtered(b"\x1b[24;80R"), b"\x1b[24;80R");
        assert_eq!(filtered(b"\x1b[?24;80;1R"), b"\x1b[?24;80;1R", "DECXCPR");
        assert_eq!(
            filtered(b"\x1b]4;1;rgb:cd00/0000/0000\x07"),
            b"\x1b]4;1;rgb:cd00/0000/0000\x07",
            "OSC 4 palette BEL"
        );
        assert_eq!(
            filtered(b"\x1b]4;255;rgb:ffff/ffff/ffff\x1b\\"),
            b"\x1b]4;255;rgb:ffff/ffff/ffff\x1b\\",
            "OSC 4 palette ST"
        );
        assert_eq!(
            filtered(b"\x1bP>|xterm.js(5.3.0)\x1b\\"),
            b"\x1bP>|xterm.js(5.3.0)\x1b\\",
            "XTVERSION"
        );
        assert_eq!(
            filtered(b"\x1bP>|SwiftTerm 1.19\x1b\\"),
            b"\x1bP>|SwiftTerm 1.19\x1b\\"
        );
    }

    #[test]
    fn strips_device_status_report() {
        assert!(filtered(b"\x1b[0n").is_empty());
        assert!(filtered(b"\x1b[?83n").is_empty());
    }

    #[test]
    fn passes_unproxied_dcs_replies_for_the_child_to_receive() {
        // The proxy passes tertiary DA, DECRQSS and XTGETTCAP queries to the
        // client, so the client's answer is the only one the child gets.
        for reply in [
            &b"\x1bP!|7E565445\x1b\\"[..],    // tertiary DA
            b"\x1bP1$r0;1m\x1b\\",            // DECRQSS answer
            b"\x1bP0$r\x1b\\",                // DECRQSS, invalid request
            b"\x1bP1+r544e=787465726d\x07",   // XTGETTCAP answer
            b"\x1bP0+r544e\x1b\\",            // XTGETTCAP, unknown cap
            b"\x1bP1+R544e=787465726d\x1b\\", // XTGETTCAP, upper-case final
        ] {
            assert_eq!(filtered(reply), reply, "{reply:?}");
        }
    }

    #[test]
    fn strips_osc_color_reports() {
        assert!(filtered(b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07").is_empty());
        assert!(filtered(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\").is_empty());
        assert!(filtered(b"\x1b]12;rgb:c7c7/c7c7/c7c7\x07").is_empty());
    }

    #[test]
    fn strips_the_newer_proxied_reply_shapes() {
        // Everything CapabilityProxy can emit must also be stripped here —
        // otherwise a client echo of a split query leaks a duplicate reply
        // into the child.
        assert!(filtered(b"\x1b[?5u").is_empty(), "kitty flag report");
        assert!(filtered(b"\x1b[?0u").is_empty(), "kitty report, no flags");
        assert!(filtered(b"\x1b[0*{").is_empty(), "macro space report");
        assert!(
            filtered(b"\x1bP7!~0000\x1b\\").is_empty(),
            "checksum report"
        );
        assert!(
            filtered(b"\x1b]12;rgb:c7c7/c7c7/c7c7\x07").is_empty(),
            "cursor colour report"
        );
    }

    #[test]
    fn response_shapes_are_stripped_only_in_their_report_form() {
        // Kitty key events are digit-led `u` — never `?`-led — so they pass.
        assert_eq!(filtered(b"\x1b[97;5u"), b"\x1b[97;5u", "key event");
        assert_eq!(
            filtered(b"\x1b[=1u"),
            b"\x1b[=1u",
            "`=`-led is not a report"
        );
        // `{` without the `*` marker is not the macro space report.
        assert_eq!(filtered(b"\x1b[5{"), b"\x1b[5{");
    }

    #[test]
    fn scan_bound_matches_the_proxy_at_the_limit() {
        // Both directions allow the terminator through index MAX_SEQUENCE_LEN:
        // a DCS checksum reply whose ST lands exactly on the bound still
        // strips; one byte longer does not.
        let mut at_limit = Vec::from(&b"\x1bP"[..]);
        at_limit.extend_from_slice(&vec![b'0'; MAX_SEQUENCE_LEN - 8]);
        at_limit.extend_from_slice(b"!~0000\x1b\\");
        assert!(filtered(&at_limit).is_empty(), "ST ESC at the bound strips");
        let mut over = Vec::from(&b"\x1bP"[..]);
        over.extend_from_slice(&vec![b'0'; MAX_SEQUENCE_LEN - 7]);
        over.extend_from_slice(b"!~0000\x1b\\");
        assert_eq!(filtered(&over), over, "ST ESC past the bound passes");
    }

    #[test]
    fn strips_response_embedded_in_typing() {
        // The realistic case: the emulator's auto-answer lands in the middle of
        // the user's keystrokes.
        assert_eq!(filtered(b"ls\x1b[?62;22c -la\r"), b"ls -la\r");
    }

    #[test]
    fn strips_multiple_responses_in_one_chunk() {
        // DA and DSR still stripped; CPR passes through untouched.
        assert_eq!(filtered(b"a\x1b[0nb\x1b[24;80Rc"), b"ab\x1b[24;80Rc");
    }

    #[test]
    fn passes_plain_text_untouched_without_allocating() {
        let mut f = TermFilter::new();
        assert!(matches!(f.filter(b"echo hello\r"), Cow::Borrowed(_)));
        assert_eq!(f.stripped_sequences(), 0);
    }

    #[test]
    fn passes_keyboard_input_unchanged() {
        for input in [
            &b"\x1b[A"[..],              // up arrow
            b"\x1b[B",                   // down
            b"\x1b[1;5C",                // ctrl+right
            b"\x1b[3~",                  // delete
            b"\x1bOP",                   // F1 via SS3
            b"\x1b",                     // bare Esc keypress
            b"\x1b\x1b",                 // Esc Esc (vi users do this)
            b"\x03",                     // Ctrl-C
            b"\x1b[97;5u",               // Kitty keyboard protocol
            b"\x1b[200~pasted\x1b[201~", // bracketed paste
        ] {
            assert_eq!(filtered(input), input, "must pass through: {input:?}");
        }
    }

    #[test]
    fn passes_mouse_and_focus_reports_unchanged() {
        for input in [
            &b"\x1b[<0;10;5M"[..], // SGR mouse press
            b"\x1b[<0;10;5m",      // SGR mouse release (lowercase m, not n)
            b"\x1b[I",             // focus in
            b"\x1b[O",             // focus out
        ] {
            assert_eq!(filtered(input), input, "must pass through: {input:?}");
        }
    }

    #[test]
    fn passes_literal_letters_that_match_response_finals() {
        // `c`, `n` and `R` only matter after a CSI introducer.
        assert_eq!(filtered(b"cat n R\r"), b"cat n R\r");
    }

    #[test]
    fn passes_unrecognised_osc_and_dcs_untouched() {
        let osc_title = b"\x1b]0;my title\x07";
        assert_eq!(filtered(osc_title), osc_title);
        let dcs_other = b"\x1bPtmux;something\x1b\\";
        assert_eq!(filtered(dcs_other), dcs_other);
    }

    #[test]
    fn passes_unterminated_sequences_untouched() {
        // Split across chunks: documented pass-through rather than holding the
        // bytes back (which would delay a bare Esc).
        assert_eq!(filtered(b"\x1b[?62;22"), b"\x1b[?62;22");
        assert_eq!(filtered(b"\x1b]11;rgb:1e1e"), b"\x1b]11;rgb:1e1e");
        assert_eq!(filtered(b"\x1bP!|7E56"), b"\x1bP!|7E56");
    }

    #[test]
    fn bounded_scan_does_not_strip_overlong_runs() {
        let mut input = Vec::from(&b"\x1b["[..]);
        input.extend_from_slice(&vec![b'1'; MAX_SEQUENCE_LEN + 8]);
        input.push(b'c');
        assert_eq!(
            filtered(&input),
            input,
            "past the scan bound: leave it alone"
        );
    }

    #[test]
    fn counters_track_what_was_removed() {
        let mut f = TermFilter::new();
        let out = f.filter(b"x\x1b[0n\x1b[24;80Ry");
        assert_eq!(out.as_ref(), b"x\x1b[24;80Ry", "CPR passes, DSR stripped");
        assert_eq!(f.stripped_sequences(), 1);
        assert_eq!(f.stripped_bytes(), 4);
    }
}
