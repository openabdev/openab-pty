//! Terminal-capability query proxy at the PTY boundary (PTY → runtime).
//!
//! To a program inside the PTY, the terminal *is* the runtime; attached clients
//! are its display. Yet a program's capability queries — Device Attributes,
//! kitty-keyboard flags, foreground/background colour — used to be relayed out
//! to whichever client happened to be attached, and the answer relayed back.
//! Three failures followed (openabdev/openab-pty#15):
//!
//!   1. A session that starts **detached** never gets an answer, so a TUI that
//!      gates rendering on DA (Devin) hangs.
//!   2. Queries live in the ring buffer, so every `?since=` replay re-delivers
//!      them; the client answers again; the original asker is gone; the shell
//!      echoes the answer as input — the `10;rgb:…65;…c0u…R` garbage.
//!   3. The client→PTY response filter had to be turned off per-variant to let
//!      Devin's own queries through, which un-fixed (2) for that variant.
//!
//! The fix is to answer **static** queries here, at the source: recognise them
//! in PTY output, emit the reply straight back to the child, and **strip them
//! from the output stream** so they never reach a client or the ring buffer.
//! Consumed at the source, a query cannot be replayed, cannot race a departed
//! reader, and needs no client attached.
//!
//! The answers mirror the reference client (SwiftTerm) exactly — the proxy must
//! never advertise a capability the display cannot honour.
//!
//! **Not proxied: CPR** (`CSI 6 n`, cursor position). That is live screen
//! state, and this runtime is a byte relay with no screen model, so it cannot
//! be answered here. CPR queries pass through untouched; the client answers
//! them under a correlation window (client side). Answering CPR at the source
//! waits on an embedded VT state machine (libghostty-vt), tracked separately.

use std::borrow::Cow;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const MAX_SEQUENCE_LEN: usize = 256;

/// A recognised query and the bytes to answer it with. `consume` is how many
/// input bytes the query occupied (stripped from the output stream).
struct Match {
    consume: usize,
    response: Vec<u8>,
}

/// Detects static capability queries in PTY output and produces the runtime's
/// answers, mirroring SwiftTerm. Stateless across chunks for the same reason
/// TermFilter is: a sequence split across two reads is rare, and holding bytes
/// back would stall live output. A split query is simply not proxied on that
/// read — the status quo, not a new failure.
#[derive(Debug, Default)]
pub struct CapabilityProxy {
    answered: u64,
}

impl CapabilityProxy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn answered(&self) -> u64 {
        self.answered
    }

    /// Scan one PTY→client chunk. Returns the chunk with recognised queries
    /// removed (borrowed and untouched in the common case) plus the bytes to
    /// write back to the child, concatenated in query order.
    pub fn process<'a>(&mut self, chunk: &'a [u8]) -> (Cow<'a, [u8]>, Vec<u8>) {
        if !chunk.contains(&ESC) {
            return (Cow::Borrowed(chunk), Vec::new());
        }
        let mut out: Option<Vec<u8>> = None;
        let mut responses: Vec<u8> = Vec::new();
        let mut i = 0;
        let mut copied_upto = 0;
        while i < chunk.len() {
            if chunk[i] != ESC {
                i += 1;
                continue;
            }
            match classify_query(&chunk[i..]) {
                Some(m) => {
                    let out = out.get_or_insert_with(|| Vec::with_capacity(chunk.len()));
                    out.extend_from_slice(&chunk[copied_upto..i]);
                    responses.extend_from_slice(&m.response);
                    self.answered += 1;
                    i += m.consume;
                    copied_upto = i;
                }
                None => i += 1,
            }
        }
        match out {
            Some(mut out) => {
                out.extend_from_slice(&chunk[copied_upto..]);
                (Cow::Owned(out), responses)
            }
            None => (Cow::Borrowed(chunk), responses),
        }
    }
}

/// If `s` begins with a static capability *query* the runtime answers, return
/// the match. CPR and anything not recognised return `None` (pass through).
fn classify_query(s: &[u8]) -> Option<Match> {
    debug_assert_eq!(s.first(), Some(&ESC));
    match s.get(1)? {
        b'[' => classify_csi_query(s),
        b']' => classify_osc_query(s),
        _ => None,
    }
}

fn classify_csi_query(s: &[u8]) -> Option<Match> {
    // Collect the private-marker + parameters, then the final byte.
    let mut i = 2;
    let mut params_start = 2;
    // Leading private marker (`?`, `>`, `=`).
    let private = s.get(2).copied().filter(|b| matches!(b, b'?' | b'>' | b'='));
    if private.is_some() {
        i = 3;
        params_start = 3;
    }
    while i < s.len() && i <= MAX_SEQUENCE_LEN {
        let b = s[i];
        match b {
            0x30..=0x3b => i += 1, // digits and ';'
            0x40..=0x7e => {
                let params = &s[params_start..i];
                let consume = i + 1;
                return match (private, b) {
                    // Primary DA: `CSI c` or `CSI 0 c`. SwiftTerm's xterm reply.
                    (None, b'c') if params.is_empty() || params == b"0" => Some(Match {
                        consume,
                        response: b"\x1b[?65;1;2;6;21;22;17;28c".to_vec(),
                    }),
                    // Secondary DA: `CSI > c` / `CSI > 0 c` -> VT525, kbd 1.
                    (Some(b'>'), b'c') if params.is_empty() || params == b"0" => Some(Match {
                        consume,
                        response: b"\x1b[>65;20;1c".to_vec(),
                    }),
                    // DSR status: `CSI 5 n` -> OK (`CSI 0 n`).
                    (None, b'n') if params == b"5" => Some(Match {
                        consume,
                        response: b"\x1b[0n".to_vec(),
                    }),
                    // Kitty keyboard query: `CSI ? u` -> flags 0.
                    (Some(b'?'), b'u') if params.is_empty() => Some(Match {
                        consume,
                        response: b"\x1b[?0u".to_vec(),
                    }),
                    // CPR (`CSI 6 n`) and everything else: not answered here.
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}

/// Colour queries: `OSC 10 ; ? ST` (fg), `OSC 11 ; ? ST` (bg). The runtime
/// forces a dark display, so it answers a fixed dark palette — truthful for
/// this client. A `?` payload marks a query; a colour payload is a response
/// (handled by the filter, not here).
fn classify_osc_query(s: &[u8]) -> Option<Match> {
    let (payload, len) = string_sequence(s, 2)?;
    // Ps ; ?
    let sep = payload.iter().position(|&b| b == b';')?;
    let (ps, rest) = (&payload[..sep], &payload[sep + 1..]);
    if rest != b"?" {
        return None;
    }
    let terminator: &[u8] = if s[len - 1] == BEL { b"\x07" } else { b"\x1b\\" };
    let colour: &[u8] = match ps {
        b"10" => b"rgb:c7c7/c7c7/c7c7", // foreground: light grey
        b"11" => b"rgb:1e1e/1e1e/1e1e", // background: near-black (dark UI)
        _ => return None,
    };
    let mut response = Vec::new();
    response.extend_from_slice(b"\x1b]");
    response.extend_from_slice(ps);
    response.push(b';');
    response.extend_from_slice(colour);
    response.extend_from_slice(terminator);
    Some(Match { consume: len, response })
}

fn string_sequence(s: &[u8], body: usize) -> Option<(&[u8], usize)> {
    let mut i = body;
    let limit = s.len().min(MAX_SEQUENCE_LEN);
    while i < limit {
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

    fn run(input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut p = CapabilityProxy::new();
        let (out, resp) = p.process(input);
        (out.to_vec(), resp)
    }

    #[test]
    fn answers_primary_da_and_strips_it() {
        let (out, resp) = run(b"before\x1b[cafter");
        assert_eq!(out, b"beforeafter", "query removed from output");
        assert_eq!(resp, b"\x1b[?65;1;2;6;21;22;17;28c");
    }

    #[test]
    fn answers_primary_da_zero_param() {
        let (_, resp) = run(b"\x1b[0c");
        assert_eq!(resp, b"\x1b[?65;1;2;6;21;22;17;28c");
    }

    #[test]
    fn answers_secondary_da() {
        let (out, resp) = run(b"\x1b[>c");
        assert!(out.is_empty());
        assert_eq!(resp, b"\x1b[>65;20;1c");
    }

    #[test]
    fn answers_dsr_status_but_not_cpr() {
        let (_, ok) = run(b"\x1b[5n");
        assert_eq!(ok, b"\x1b[0n");
        // CPR must pass through untouched and produce no runtime answer.
        let (out, cpr) = run(b"\x1b[6n");
        assert_eq!(out, b"\x1b[6n");
        assert!(cpr.is_empty());
    }

    #[test]
    fn answers_kitty_keyboard_query() {
        let (out, resp) = run(b"\x1b[?u");
        assert!(out.is_empty());
        assert_eq!(resp, b"\x1b[?0u");
    }

    #[test]
    fn answers_osc_colour_queries_with_dark_palette() {
        let (out, fg) = run(b"\x1b]10;?\x07");
        assert!(out.is_empty());
        assert_eq!(fg, b"\x1b]10;rgb:c7c7/c7c7/c7c7\x07");
        let (_, bg) = run(b"\x1b]11;?\x1b\\");
        assert_eq!(bg, b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\");
    }

    #[test]
    fn does_not_answer_osc_colour_reports() {
        // A colour *value* (not `?`) is a response, not a query — pass through.
        let report = b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07";
        let (out, resp) = run(report);
        assert_eq!(out, report);
        assert!(resp.is_empty());
    }

    #[test]
    fn the_devin_startup_burst_is_answered_and_stripped() {
        // DA + kitty + colour in one read, interleaved with real output.
        let input = b"\x1b[c\x1b[?u\x1b]11;?\x07shell$ ";
        let (out, resp) = run(input);
        assert_eq!(out, b"shell$ ", "only real output survives");
        assert_eq!(
            resp,
            b"\x1b[?65;1;2;6;21;22;17;28c\x1b[?0u\x1b]11;rgb:1e1e/1e1e/1e1e\x07"
        );
    }

    #[test]
    fn plain_output_is_untouched_and_borrowed() {
        let mut p = CapabilityProxy::new();
        let (out, resp) = p.process(b"just some log output\r\n");
        assert!(matches!(out, Cow::Borrowed(_)));
        assert!(resp.is_empty());
        assert_eq!(p.answered(), 0);
    }

    #[test]
    fn arrow_keys_and_mouse_are_not_queries() {
        for input in [&b"\x1b[A"[..], b"\x1b[3~", b"\x1b[<0;1;1M", b"\x1b[I"] {
            let (out, resp) = run(input);
            assert_eq!(out, input);
            assert!(resp.is_empty(), "not a query: {input:?}");
        }
    }

    #[test]
    fn split_query_passes_through() {
        let (out, resp) = run(b"\x1b[?62;22");
        assert_eq!(out, b"\x1b[?62;22");
        assert!(resp.is_empty());
    }
}
