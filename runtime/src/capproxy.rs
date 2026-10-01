//! Terminal-capability query proxy at the PTY boundary (PTY → runtime).
//!
//! A terminal app usually probes the display with a handful of capability
//! queries at startup — device attributes (`CSI c` / `CSI > c`),
//! device-status reports (`CSI 5 n` and the DEC private `?` family), the
//! kitty keyboard query (`CSI ? u`) and OSC colour queries (10/11/12). Inside openab the bytes reach a WebSocket client
//! (SwiftTerm), not a local display — and with no client attached they would
//! reach nobody at all.
//!
//! Leaving queries on the wire is doubly lossy: they land in the replay
//! buffer where they can be answered a second time on every attach, and the
//! client's own answer takes a full network round trip while the runtime
//! already knows it. So the runtime answers the static queries at the source
//! — the answers mirror the reference client (SwiftTerm) — and strips them
//! from the stream, so neither the ring buffer nor any attached client ever
//! sees them.
//!
//! **Kitty keyboard flags are stateful.** A static `?0u` answer would tell an
//! app that pushed `CSI > flags u` that its push failed. The proxy therefore
//! watches the client's own state changes go by — `CSI >` push, `CSI <` pop,
//! `CSI = flags ; mode u` set — and answers `CSI ? u` with the flags the
//! client actually applied. Like SwiftTerm, it keeps one stack per screen
//! (normal/alternate, switched by `CSI ? 47|1047|1049 h|l`, cleared along
//! with the whole model by `ESC c`), bounded at 16 entries with the five
//! known flag bits.
//!
//! The client's emulator is not only moved by these bytes, though. The
//! runtime *assumes* a client that starts a fresh emulator on an attach with no
//! cursor, and resets it (RIS) on a `gap` frame — the latter is what Connect
//! does; neither shipped client attaches without a cursor today. The
//! session therefore calls [`CapabilityProxy::resync`] at attach in exactly
//! those two cases — reset, then re-observe the replay bytes the client is
//! about to be fed — so the two models start from the same place. (An attach
//! with `since=0` and nothing evicted is a contiguous replay of the whole
//! stream: a fresh emulator fed it ends where the proxy already is.)
//!
//! Not mirrored: a client that handles a gap without resetting its emulator
//! (the iPhone client only prints a marker today); a `gap` raised mid-stream by
//! a slow client's backlog overflowing (rare, one step from a `SLOW_CLIENT`
//! close); and the attach handoff's chunk-boundary race (a chunk the proxy has
//! seen but the ring has not yet stored when the snapshot is taken). After any
//! of these, `CSI ? u` can disagree with the client until the app sets its
//! flags again — no worse than the static `?0u` this replaced.
//!
//! **Parameters are numeric.** `CSI 00 c` and `CSI 05 n` are the same queries
//! as `CSI c` / `CSI 5 n` — SwiftTerm's parser accumulates digits, so the
//! proxy compares parsed values rather than byte patterns. A sequence with
//! more than 24 parameter groups is one SwiftTerm refuses to dispatch at all,
//! so the proxy neither answers it nor lets it move kitty state.
//!
//! **OSC queries.** Colour OSCs answer per `?` group like the client:
//! `OSC 10 ; ? ; ?` is a foreground-then-background query and gets two
//! replies. Any group that is a colour *set* makes the whole sequence a
//! passthrough: answering only the `?` parts would drop the set the client
//! was meant to apply.
//!
//! **Only what the client would say.** Every answer here is one the reference
//! client actually sends, checked against SwiftTerm 1.19.0 (and 1.18.0, the
//! iPhone pin). A query the client does not answer — tertiary DA (`CSI = c`),
//! colour-scheme DSR (`CSI ? 996 n`), `CSI ? 998 n` — is passed through, not
//! invented; so is `OSC 4`, because the client's palette (Terminal.app's 16
//! plus a Lab-derived 256) is not the xterm one and the proxy has no copy of
//! it. A wrong answer is worse than none: it tells an app about a capability,
//! or a colour, the screen does not have.
//!
//! **Terminators.** OSC replies reuse the query's own terminator (BEL → BEL,
//! ST → ST). That is xterm's convention rather than SwiftTerm's — which
//! always sends ST — and the module deliberately keeps it: either terminator
//! ends the reply correctly for the querying application, and echoing the
//! query's framing is the least surprising relay.
//!
//! **Colours.** The runtime forces a dark display, so the colour answers are
//! the declared palette: foreground `c7c7c7`, background `1e1e1e`, and the
//! cursor colour reported as the foreground — SwiftTerm's default when no
//! cursor colour is set. Colour *sets* are not mirrored, so a query after a set reports the
//! declared palette — the same approximation the fixed foreground/background
//! answers already made.
//!
//! What deliberately still passes through:
//!
//! - **CPR** (`CSI 6 n`) and its DEC form (`CSI ? 6 n`). Answering needs
//!   live cursor state — a screen model, not a byte filter. Follow-up: a
//!   thin VT-state reader (libghostty-vt or equivalent) so even this is
//!   answered at the source without a client.
//! - **Colour sets, `OSC 4`, tertiary DA and unrecognised DSR `?` queries**
//!   — the client owns them; the filter on the input side drops any echo
//!   answers.
//!
//! The filter direction is the mirror of this proxy — `termfilter.rs`
//! answers the "who ate my reply" question by deleting client→PTY copies of
//! the same responses.
//!
//! **Chunk-scoped.** `process` never buffers across reads: a sequence split
//! across two PTY reads is not recognised and passes through — which is
//! *not* benign for a query: the client still answers it, but the input
//! filter deletes that reply (it strips every response the proxy itself
//! would have sent), so a split query gets no answer at all. The kitty
//! tracking has the same limit — a push split across reads is not seen, so
//! a later query may under-report until the client moves the state again.

use std::borrow::Cow;
use std::io::Write as _;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// Upper bound on a single recognised sequence. Real queries are far
/// shorter; this only keeps the scan from running unbounded.
const MAX_SEQUENCE_LEN: usize = 256;

/// Kitty keyboard-protocol limits, matching the client: a 16-entry stack
/// and the five flags kitty defines (disambiguate, report events, report
/// alternates, report all keys, report text).
const KITTY_STACK_LIMIT: usize = 16;
const KITTY_KNOWN_MASK: u16 = 0x1f;

/// The dark palette the runtime declares for the client's defaults.
const FG_COLOUR: &[u8] = b"rgb:c7c7/c7c7/c7c7";
const BG_COLOUR: &[u8] = b"rgb:1e1e/1e1e/1e1e";
/// SwiftTerm reports the cursor colour as the foreground when the app never
/// set one — and nothing here ever sets one.
const CURSOR_COLOUR: &[u8] = FG_COLOUR;

struct Match {
    consume: usize,
    response: Vec<u8>,
}

/// Outcome of looking at one `ESC`-led slice. `Answer` is consumed by the
/// proxy; `Observed` passes through (state tracked where relevant); the
/// length returned with it is the parsed sequence, so scanning can skip its
/// body rather than re-testing every byte.
enum Inspection {
    Answer(Match),
    Observed(usize),
    Unknown,
}

/// Kitty keyboard state for one screen. SwiftTerm keeps this per buffer —
/// pushing on the normal screen must not leak into the alternate.
#[derive(Debug, Default)]
struct KittyScreen {
    flags: u16,
    stack: Vec<u16>,
}

pub struct CapabilityProxy {
    answered: u64,
    /// `[normal, alternate]` kitty stacks — the client's two screens.
    kitty: [KittyScreen; 2],
    alt_active: bool,
}

impl CapabilityProxy {
    pub fn new() -> Self {
        Self {
            answered: 0,
            kitty: [KittyScreen::default(), KittyScreen::default()],
            alt_active: false,
        }
    }

    /// One PTY-output chunk in → (output worth keeping, bytes to write back
    /// to the child). Recognised queries are removed from the kept output
    /// and their answers concatenated in order.
    ///
    /// The strip is best-effort: a sequence split across chunks is passed
    /// through — and the kitty tracking follows the same rule, so a split
    /// push/pop/set is simply not seen.
    pub fn process<'a>(&mut self, chunk: &'a [u8]) -> (Cow<'a, [u8]>, Vec<u8>) {
        // Fast path: most output has no ESC at all.
        if !chunk.contains(&ESC) {
            return (Cow::Borrowed(chunk), Vec::new());
        }

        let mut out: Option<Vec<u8>> = None;
        let mut responses: Vec<u8> = Vec::new();
        let mut i = 0usize;
        let mut copied_upto = 0usize;
        while i < chunk.len() {
            if chunk[i] != ESC {
                i += 1;
                continue;
            }
            match self.inspect(&chunk[i..]) {
                Inspection::Answer(m) => {
                    let out = out.get_or_insert_with(|| Vec::with_capacity(chunk.len()));
                    out.extend_from_slice(&chunk[copied_upto..i]);
                    responses.extend_from_slice(&m.response);
                    self.answered += 1;
                    i += m.consume;
                    copied_upto = i;
                }
                Inspection::Observed(len) => i += len,
                Inspection::Unknown => i += 1,
            }
        }
        match out {
            Some(mut out) => {
                out.extend_from_slice(&chunk[copied_upto..]);
                (Cow::Owned(out), responses)
            }
            None => (Cow::Borrowed(chunk), Vec::new()),
        }
    }

    /// Observability hook: how many queries this proxy handled — answered,
    /// or deliberately consumed without a reply (e.g. an `OSC 10` whose groups
    /// are all client-side no-ops, where the client itself would stay silent).
    pub fn answered(&self) -> u64 {
        self.answered
    }

    /// Bring the kitty model in line with a client that is about to start from
    /// a fresh (or freshly reset) emulator and be fed `replay`: reset, then
    /// observe `replay` for state changes only. Nothing is answered or counted
    /// — those queries were answered when the bytes first went by, and the
    /// replay never reaches the child.
    pub fn resync(&mut self, replay: &[u8]) {
        self.reset_emulator_model();
        let answered = self.answered;
        let _ = self.process(replay);
        self.answered = answered;
    }

    /// What `ESC c` does to the client: both kitty stacks cleared, back on the
    /// normal screen.
    fn reset_emulator_model(&mut self) {
        self.kitty = [KittyScreen::default(), KittyScreen::default()];
        self.alt_active = false;
    }

    fn inspect(&mut self, s: &[u8]) -> Inspection {
        debug_assert_eq!(s.first(), Some(&ESC));
        match s.get(1) {
            Some(b'[') => self.inspect_csi(s),
            Some(b']') => match classify_osc_query(s) {
                Some(m) => Inspection::Answer(m),
                None => Inspection::Unknown,
            },
            // RIS — the client resets everything, including both kitty
            // stacks and the screen selection. The sequence still passes
            // through; only the model here is cleared.
            Some(b'c') => {
                self.reset_emulator_model();
                Inspection::Observed(2)
            }
            _ => Inspection::Unknown,
        }
    }

    fn inspect_csi(&mut self, s: &[u8]) -> Inspection {
        let mut i = 2usize;
        let private = s
            .get(2)
            .copied()
            .filter(|b| matches!(b, b'?' | b'>' | b'=' | b'<'));
        let params_start = if private.is_some() {
            i = 3;
            3
        } else {
            2
        };
        // A `:` straight after `CSI` is a parse error in the client's entry
        // state (the sequence aborts and the final byte is printed), so it is
        // no query there and none here. After a private marker the client is
        // already in its parameter state, where `:` separates like `;`.
        if private.is_none() && s.get(2) == Some(&b':') {
            return Inspection::Unknown;
        }
        while i < s.len() && i <= MAX_SEQUENCE_LEN {
            let b = s[i];
            match b {
                // Parameter bytes: digits, ':' (sub-parameter separator in
                // the client's parser) and ';'.
                0x30..=0x3b => i += 1,
                // Final byte.
                0x40..=0x7e => {
                    return self.dispatch_csi(private, &s[params_start..i], b, i + 1);
                }
                _ => return Inspection::Unknown,
            }
        }
        Inspection::Unknown
    }

    /// A fully-parsed CSI: either a query this layer answers, or a passthrough
    /// whose effect on the client's state is tracked where it matters.
    fn dispatch_csi(
        &mut self,
        private: Option<u8>,
        params: &[u8],
        final_byte: u8,
        len: usize,
    ) -> Inspection {
        // Past 24 groups SwiftTerm skips dispatch entirely: no answer, no
        // state change. Mirror that rather than act on a truncated list.
        let Some((pars, count)) = parse_params(params) else {
            return Inspection::Observed(len);
        };
        let p0 = pars[0];
        match (private, final_byte) {
            // Primary DA — `CSI [0] c`, including zero-padded forms.
            // `;4` is sixel: SwiftTerm's `enableSixelReported` defaults on and
            // no client of this runtime turns it off (#51).
            (None, b'c') if p0 == 0 => {
                Inspection::Answer(answer(len, b"\x1b[?65;4;1;2;6;21;22;17;28c".to_vec()))
            }
            // Secondary DA — `CSI > [0] c`.
            (Some(b'>'), b'c') if p0 == 0 => {
                Inspection::Answer(answer(len, b"\x1b[>65;20;1c".to_vec()))
            }
            // Tertiary DA (`CSI = c`) is deliberately absent: SwiftTerm does
            // not answer it, so neither does the proxy.
            // DSR "terminal ready". `6` (CPR) is deliberately absent: it
            // needs live cursor state the proxy does not have.
            (None, b'n') if p0 == 5 => Inspection::Answer(answer(len, b"\x1b[0n".to_vec())),
            // DEC private DSR family — the fixed answers SwiftTerm emits.
            (Some(b'?'), b'n') => self.dsr_private(&pars[..count], len),
            // Kitty keyboard query — report the flags the client applied.
            (Some(b'?'), b'u') => {
                let flags = self.kitty_screen().flags;
                let mut response = Vec::with_capacity(8);
                let _ = write!(response, "\x1b[?{flags}u");
                Inspection::Answer(answer(len, response))
            }
            // Kitty keyboard state changes — the client owns them, the
            // proxy watches so its `?u` answer stays true.
            (Some(b'>'), b'u') => {
                self.kitty_push(p0);
                Inspection::Observed(len)
            }
            (Some(b'='), b'u') => {
                self.kitty_set(p0, if count > 1 { pars[1] } else { 1 });
                Inspection::Observed(len)
            }
            (Some(b'<'), b'u') => {
                self.kitty_pop(p0);
                Inspection::Observed(len)
            }
            // DECSET/DECRST — the buffer-switch modes move kitty state
            // between the two screens.
            (Some(b'?'), b'h') => {
                self.buffer_modes(&pars[..count], true);
                Inspection::Observed(len)
            }
            (Some(b'?'), b'l') => {
                self.buffer_modes(&pars[..count], false);
                Inspection::Observed(len)
            }
            _ => Inspection::Observed(len),
        }
    }

    /// `CSI ? Ps n` — the private statuses SwiftTerm answers. Anything not
    /// in the table (including `?6` — DECXCPR — and `?6`-like screen queries)
    /// stays a passthrough.
    fn dsr_private(&self, pars: &[u16], len: usize) -> Inspection {
        let response: Vec<u8> = match pars[0] {
            15 => b"\x1b[?10n".to_vec(),       // printer ready
            25 => b"\x1b[?21n".to_vec(),       // UDKs locked
            26 => b"\x1b[?27;1;0;0n".to_vec(), // North-American keyboard
            55 => b"\x1b[?53n".to_vec(),       // locator available
            56 => b"\x1b[?57;1n".to_vec(),     // locator is a mouse
            62 => b"\x1b[0*{".to_vec(),        // macro space report
            63 => {
                // Macro checksum carries the requested memory id through.
                let id = pars.get(1).copied().unwrap_or(0);
                let mut v = Vec::with_capacity(12);
                let _ = write!(v, "\x1bP{id}!~0000\x1b\\");
                v
            }
            75 => b"\x1b[?70n".to_vec(), // data integrity OK
            85 => b"\x1b[?83n".to_vec(), // single session
            // Not 996 (colour-scheme report) or 998: SwiftTerm does not
            // answer them, and a reply would claim a feature it lacks.
            _ => return Inspection::Observed(len),
        };
        Inspection::Answer(answer(len, response))
    }

    fn kitty_screen(&mut self) -> &mut KittyScreen {
        &mut self.kitty[usize::from(self.alt_active)]
    }

    fn kitty_push(&mut self, raw_flags: u16) {
        let screen = self.kitty_screen();
        if screen.stack.len() >= KITTY_STACK_LIMIT {
            screen.stack.remove(0);
        }
        screen.stack.push(screen.flags);
        screen.flags = raw_flags & KITTY_KNOWN_MASK;
    }

    /// `CSI = flags ; mode u` — mode 1 assigns, 2 unions, 3 subtracts;
    /// anything else is ignored, exactly like the client.
    fn kitty_set(&mut self, raw_flags: u16, mode: u16) {
        let new_flags = raw_flags & KITTY_KNOWN_MASK;
        let screen = self.kitty_screen();
        match mode {
            1 => screen.flags = new_flags,
            2 => screen.flags |= new_flags,
            3 => screen.flags &= !new_flags,
            _ => {}
        }
    }

    /// `CSI < [count] u` — pop count entries (default 1); past the bottom
    /// clears the stack and the flags, per the client.
    fn kitty_pop(&mut self, count: u16) {
        let count = usize::from(count).max(1);
        let screen = self.kitty_screen();
        if count > screen.stack.len() {
            screen.stack.clear();
            screen.flags = 0;
            return;
        }
        for _ in 0..count {
            if let Some(flags) = screen.stack.pop() {
                screen.flags = flags;
            }
        }
    }

    /// `CSI ? … h/l` — 47, 1047 and 1049 move between normal and alternate
    /// screens. (1048 is cursor save/restore only and is not tracked.)
    fn buffer_modes(&mut self, pars: &[u16], set: bool) {
        if pars.iter().any(|m| matches!(m, 47 | 1047 | 1049)) {
            self.alt_active = set;
        }
    }
}

impl Default for CapabilityProxy {
    fn default() -> Self {
        Self::new()
    }
}

fn answer(consume: usize, response: Vec<u8>) -> Match {
    Match { consume, response }
}

/// Parse a CSI parameter list: `;` and `:` both open a new slot, empty slots
/// are 0, values saturate at 65535, and more than 24 slots is `None` — the
/// client's parser refuses to dispatch such a sequence. An empty parameter list
/// still yields `[0]`. A `:` leading a non-private CSI never gets here — see
/// `inspect_csi`.
fn parse_params(params: &[u8]) -> Option<([u16; 24], usize)> {
    let mut pars = [0u16; 24];
    let mut count = 0usize;
    for group in params.split(|b| *b == b';' || *b == b':') {
        if count == pars.len() {
            return None;
        }
        let mut value: u32 = 0;
        for &digit in group {
            value = (value * 10 + u32::from(digit - b'0')).min(u32::from(u16::MAX));
        }
        pars[count] = value as u16;
        count += 1;
    }
    Some((pars, count))
}

/// `ESC ] …` — returns (payload, total length) when the slice holds a
/// complete OSC terminated by BEL or ST (`ESC \`).
fn string_sequence(s: &[u8], code_offset: usize) -> Option<(&[u8], usize)> {
    let mut i = code_offset;
    while i < s.len() && i <= MAX_SEQUENCE_LEN {
        match s[i] {
            BEL => return Some((&s[code_offset..i], i + 1)),
            ESC => {
                if s.get(i + 1) == Some(&b'\\') {
                    return Some((&s[code_offset..i], i + 2));
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Strict decimal — all bytes digits, non-empty, no overflow — matching the
/// client's `parseDecimal`. `None` means "not a number", which upstream
/// treats as an ignored OSC code rather than 0.
fn parse_decimal(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(value)
}

/// `ESC ] Ps ; …` colour queries. Only the `?` forms are answered here;
/// anything carrying a colour value is a set for the client and passes
/// through untouched (a mixed query+set sequence passes through whole, so
/// the set is never dropped).
fn classify_osc_query(s: &[u8]) -> Option<Match> {
    let (payload, len) = string_sequence(s, 2)?;
    let terminator: &[u8] = if s[len - 1] == BEL {
        b"\x07"
    } else {
        b"\x1b\\"
    };
    let sep = payload.iter().position(|&b| b == b';')?;
    let ps = parse_decimal(&payload[..sep])?;
    let rest = &payload[sep + 1..];
    match ps {
        // Not 4 (palette): see the module doc — the client's palette is not
        // one the proxy can report.
        10..=12 => osc_face_colour(ps - 10, rest, len, terminator),
        _ => None,
    }
}

/// `OSC 10/11/12 ; text ; text…` — SwiftTerm walks the groups and, starting
/// at the code's own slot, queries foreground/background/cursor per `?`
/// group. Groups beyond the cursor slot are ignored either way; a parseable
/// colour in a live slot is a set and makes the whole sequence passthrough.
fn osc_face_colour(base: u32, rest: &[u8], len: usize, terminator: &[u8]) -> Option<Match> {
    let mut response = Vec::new();
    // Empty groups are dropped before indexing, as Swift's `split` does:
    // `OSC 10;?;;?` is fg then bg there, not fg then cursor.
    for (offset, group) in rest
        .split(|&b| b == b';')
        .filter(|group| !group.is_empty())
        .enumerate()
    {
        let target = base as usize + offset;
        if group.first() == Some(&b'?') {
            let colour = match target {
                0 => FG_COLOUR,
                1 => BG_COLOUR,
                2 => CURSOR_COLOUR,
                _ => continue,
            };
            push_colour_report(&mut response, 10 + target as u32, colour, terminator);
        } else if target <= 2 && is_colour(group) {
            return None;
        }
    }
    // Every group was a query or a client-side no-op — the sequence is
    // answered (or at least consumed) here, never replayed for nothing.
    Some(answer(len, response))
}

fn push_colour_report(out: &mut Vec<u8>, code: u32, colour: &[u8], terminator: &[u8]) {
    out.extend_from_slice(b"\x1b]");
    let _ = write!(out, "{code};");
    out.extend_from_slice(colour);
    out.extend_from_slice(terminator);
}

/// The colour forms the client's `parseColor` accepts — `#` + 3/6/9/12 hex
/// digits (junk hex still parses as a colour there) and a case-sensitive
/// `rgb:` prefix with at least one hex run. Anything else it parses to
/// `nil`, so the sequence is treated as a no-op group rather than a set.
fn is_colour(spec: &[u8]) -> bool {
    if spec.first() == Some(&b'#') {
        matches!(spec.len() - 1, 3 | 6 | 9 | 12)
    } else if spec.starts_with(b"rgb:") {
        let mut idx = 4usize;
        for _ in 0..3 {
            if hex_digits(spec, &mut idx) > 0 {
                return true;
            }
        }
        false
    } else {
        false
    }
}

/// Port of the client's `parseHex` walk: consume up to four hex digits,
/// stop on (and consume) a single '/', return the digit count.
fn hex_digits(data: &[u8], idx: &mut usize) -> usize {
    let mut count = 0;
    while count < 4 && *idx < data.len() {
        let c = data[*idx];
        *idx += 1;
        if c.is_ascii_hexdigit() {
            count += 1;
        } else {
            break;
        }
    }
    if *idx < data.len() && data[*idx] == b'/' {
        *idx += 1;
    }
    count
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
        let (out, resp) = run(b"\x1b[c");
        assert!(out.is_empty());
        assert_eq!(resp, b"\x1b[?65;4;1;2;6;21;22;17;28c");
    }

    #[test]
    fn answers_primary_da_zero_param() {
        let (out, resp) = run(b"\x1b[0c");
        assert!(out.is_empty());
        assert_eq!(resp, b"\x1b[?65;4;1;2;6;21;22;17;28c");
    }

    #[test]
    fn answers_secondary_da() {
        let (out, resp) = run(b"\x1b[>c");
        assert!(out.is_empty());
        assert_eq!(resp, b"\x1b[>65;20;1c");
    }

    #[test]
    fn answers_dsr_status_but_not_cpr() {
        let (out, resp) = run(b"\x1b[5n");
        assert!(out.is_empty());
        assert_eq!(resp, b"\x1b[0n");

        // CPR is cursor state, not a static answer — it must still reach
        // the client, which owns the screen model.
        let (out, resp) = run(b"\x1b[6n");
        assert_eq!(out, b"\x1b[6n");
        assert!(resp.is_empty());
    }

    #[test]
    fn answers_kitty_keyboard_query() {
        let (out, resp) = run(b"\x1b[?u");
        assert!(out.is_empty());
        assert_eq!(resp, b"\x1b[?0u");
    }

    #[test]
    fn answers_osc_colour_queries_with_dark_palette() {
        // The runtime forces a dark display; the client answers the same.
        let (out, resp) = run(b"\x1b]10;?\x07");
        assert!(out.is_empty());
        assert_eq!(resp, b"\x1b]10;rgb:c7c7/c7c7/c7c7\x07");

        let (out, resp) = run(b"\x1b]11;?\x1b\\");
        assert!(out.is_empty());
        assert_eq!(resp, b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\");
    }

    #[test]
    fn does_not_answer_osc_colour_reports() {
        // `OSC 11 ; rgb:…` is a set-command, not a query — it must reach the
        // client unchanged, and must not be eaten as a question.
        let input = b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\";
        let (out, resp) = run(input);
        assert_eq!(out, input.to_vec());
        assert!(resp.is_empty());
    }

    #[test]
    fn the_devin_startup_burst_is_answered_and_stripped() {
        // Codex's PTY layer fires this probe the moment the child opens.
        // It used to sit in the replay buffer and double-answer clients.
        let input = b"\x1b[c\x1b[?u\x1b]11;?\x1b\\";
        let (out, resp) = run(input);
        assert!(out.is_empty());
        assert_eq!(
            resp,
            b"\x1b[?65;4;1;2;6;21;22;17;28c\x1b[?0u\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"
        );
    }

    #[test]
    fn plain_output_is_untouched_and_borrowed() {
        let input = b"normal shell output \x1b[32mwith colour\x1b[0m";
        let (out, resp) = run(input);
        assert_eq!(out, input.to_vec());
        assert!(resp.is_empty());
    }

    #[test]
    fn arrow_keys_and_mouse_are_not_queries() {
        let input = b"\x1b[A\x1b[<0;1;1M";
        let (out, resp) = run(input);
        assert_eq!(out, input.to_vec());
        assert!(resp.is_empty());
    }

    #[test]
    fn split_query_passes_through() {
        // Chunk-scoped: the tail arriving in a later read is not a query.
        let (out, resp) = run(b"\x1b[?62;22");
        assert_eq!(out, b"\x1b[?62;22");
        assert!(resp.is_empty());
    }
}
