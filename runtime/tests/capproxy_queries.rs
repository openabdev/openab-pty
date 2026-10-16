//! Capability-proxy coverage tests (#18): the stateful kitty-keyboard flag
//! stack and the remaining static queries the runtime answers at the source.
//!
//! Every expectation mirrors the reference client (SwiftTerm): the kitty flag
//! stack is per-screen (normal/alternate), DSR `?`-family values are the ones
//! SwiftTerm emits, and OSC colour answers carry the runtime's declared dark
//! palette. Queries SwiftTerm does not answer are passed through, never
//! invented.

use openab_pty::capproxy::CapabilityProxy;

/// One chunk through a fresh proxy: (forwarded output, bytes answered to the
/// child). Mirrors the `run` helper inside `src/capproxy.rs`.
fn run(input: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut p = CapabilityProxy::new();
    let (out, resp) = p.process(input);
    (out.to_vec(), resp)
}

/// Feed `input` to an existing proxy, asserting it passes through untouched
/// and draws no answer — the contract for a tracked-but-not-proxied sequence.
fn passthrough(p: &mut CapabilityProxy, input: &[u8]) {
    let (out, resp) = p.process(input);
    assert_eq!(
        out.as_ref(),
        input,
        "sequence must reach the client: {input:?}"
    );
    assert!(resp.is_empty(), "no runtime answer for: {input:?}");
}

// ---------------------------------------------------------------------------
// Kitty keyboard flag state — the proxy tracks push/pop/set so `CSI ? u`
// reports the flags the client actually applied.
// ---------------------------------------------------------------------------

#[test]
fn kitty_push_then_query_reports_the_pushed_flags() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>5u");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?5u", "query must report the pushed flags");
}

#[test]
fn kitty_pop_restores_the_previous_flags() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>5u");
    passthrough(&mut p, b"\x1b[>9u");
    passthrough(&mut p, b"\x1b[<u");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?5u");
}

#[test]
fn kitty_pop_with_count_and_over_pop() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>1u");
    passthrough(&mut p, b"\x1b[>2u");
    passthrough(&mut p, b"\x1b[>4u");
    // `CSI < 2 u` pops two entries: flags land on the second-deepest value.
    passthrough(&mut p, b"\x1b[<2u");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?1u");
    // Popping past the bottom clears the stack and flags (SwiftTerm).
    passthrough(&mut p, b"\x1b[>7u");
    passthrough(&mut p, b"\x1b[<99u");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?0u");
}

#[test]
fn kitty_set_modes_assign_union_and_subtract() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[=3u"); // mode 1 (default): flags = 3
    passthrough(&mut p, b"\x1b[=6;2u"); // mode 2: union -> 3 | 6 = 7
    passthrough(&mut p, b"\x1b[=5;3u"); // mode 3: subtract -> 7 & !5 = 2
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?2u");
    // An invalid mode is ignored entirely.
    passthrough(&mut p, b"\x1b[=0;4u");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?2u");
}

#[test]
fn kitty_flags_are_masked_to_the_known_bits() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>255u");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?31u", "only the five known flags survive");
}

#[test]
fn kitty_stack_is_bounded_at_sixteen_entries() {
    let mut p = CapabilityProxy::new();
    // 17 pushes evict the oldest entry (SwiftTerm caps the stack at 16).
    for flags in 1u8..=17 {
        passthrough(&mut p, format!("\x1b[>{flags}u").as_bytes());
    }
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?17u");
    // One pop restores the 16th pushed value, not the evicted first.
    passthrough(&mut p, b"\x1b[<u");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?16u");
}

#[test]
fn kitty_state_is_per_screen_buffer() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>5u"); // normal screen: flags 5
    passthrough(&mut p, b"\x1b[?1049h"); // -> alternate screen (fresh stack)
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?0u", "the alt screen starts with no flags");
    passthrough(&mut p, b"\x1b[>9u"); // alt screen: flags 9
    passthrough(&mut p, b"\x1b[?1049l"); // -> normal screen
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?5u", "the normal screen kept its own state");
    passthrough(&mut p, b"\x1b[?1049h");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?9u", "the alt screen kept its own state");
}

#[test]
fn kitty_buffer_switch_modes_47_1047_and_1048_is_cursor_only() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>5u");
    passthrough(&mut p, b"\x1b[?1047h"); // plain alt-buffer switch
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?0u");
    passthrough(&mut p, b"\x1b[>9u");
    passthrough(&mut p, b"\x1b[?47l"); // ?47l also returns to normal
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?5u");
    // 1048 saves/restores the cursor only — it must not switch buffers.
    passthrough(&mut p, b"\x1b[?1048h");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?5u", "1048 never touches the buffer");
}

#[test]
fn kitty_pop_zero_is_one_and_empty_mode_is_invalid() {
    // An explicit `0` is not a special case: the client clamps to 1.
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>5u");
    passthrough(&mut p, b"\x1b[>9u");
    passthrough(&mut p, b"\x1b[<0u");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?5u", "CSI < 0 u pops one, like the client");
    // `CSI = 3 ; u` leaves the mode slot empty — parsed as 0, an invalid
    // mode the client ignores rather than defaulting to assign.
    passthrough(&mut p, b"\x1b[=7;u");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(
        resp, b"\x1b[?5u",
        "empty mode slot must be ignored, not assign"
    );
}

#[test]
fn kitty_state_survives_decstr_but_not_full_reset() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>5u");
    passthrough(&mut p, b"\x1b[!p"); // DECSTR leaves keyboard modes alone
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?5u");
    passthrough(&mut p, b"\x1bc"); // RIS clears both stacks and the buffer
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?0u");
}

#[test]
fn kitty_query_ignores_parameters_like_the_client() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>5u");
    let (_, resp) = p.process(b"\x1b[?1u");
    assert_eq!(
        resp, b"\x1b[?5u",
        "SwiftTerm ignores the query's parameters"
    );
}

// ---------------------------------------------------------------------------
// Numeric parameters — SwiftTerm parses `00`/`05` numerically; the proxy used
// to byte-compare and missed them.
// ---------------------------------------------------------------------------

#[test]
fn zero_padded_parameters_are_answered() {
    let (_, resp) = run(b"\x1b[00c");
    assert_eq!(resp, b"\x1b[?65;4;1;2;6;21;22;17;28c");
    let (_, resp) = run(b"\x1b[05n");
    assert_eq!(resp, b"\x1b[0n");
    let (_, resp) = run(b"\x1b[>00c");
    assert_eq!(resp, b"\x1b[>65;20;1c");
}

#[test]
fn da_extra_parameters_do_not_suppress_the_answer() {
    // pars[0] == 0 is all SwiftTerm checks; `CSI 0;7 c` still answers.
    let (_, resp) = run(b"\x1b[0;7c");
    assert_eq!(resp, b"\x1b[?65;4;1;2;6;21;22;17;28c");
    // …but a non-zero first parameter is not a DA request.
    let (out, resp) = run(b"\x1b[1c");
    assert_eq!(out, b"\x1b[1c");
    assert!(resp.is_empty());
}

// ---------------------------------------------------------------------------
// DSR `?`-family — the static status answers SwiftTerm gives. `?6` (DECXCPR)
// is cursor position: still not proxied (no screen model).
// ---------------------------------------------------------------------------

#[test]
fn answers_dec_private_dsr_family() {
    for (query, want) in [
        (&b"\x1b[?15n"[..], &b"\x1b[?10n"[..]), // printer ready
        (b"\x1b[?25n", b"\x1b[?21n"),           // UDKs locked
        (b"\x1b[?26n", b"\x1b[?27;1;0;0n"),     // North-American keyboard
        (b"\x1b[?55n", b"\x1b[?53n"),           // locator available
        (b"\x1b[?56n", b"\x1b[?57;1n"),         // locator = mouse
        (b"\x1b[?62n", b"\x1b[0*{"),            // macro space report
        (b"\x1b[?75n", b"\x1b[?70n"),           // data integrity OK
        (b"\x1b[?85n", b"\x1b[?83n"),           // single session
    ] {
        let (out, resp) = run(query);
        assert!(out.is_empty(), "{query:?} must be consumed");
        assert_eq!(resp, want, "wrong answer for {query:?}");
    }
    // Macro checksum carries the requested id through — and defaults it to
    // 0 when the query leaves the second parameter slot empty.
    let (_, resp) = run(b"\x1b[?63;7n");
    assert_eq!(resp, b"\x1bP7!~0000\x1b\\");
    let (_, resp) = run(b"\x1b[?63n");
    assert_eq!(resp, b"\x1bP0!~0000\x1b\\");
}

#[test]
fn decxpr_and_unknown_dsr_pass_through() {
    // `?6n` is a cursor-position request: needs live screen state, untouched.
    let (out, resp) = run(b"\x1b[?6n");
    assert_eq!(out, b"\x1b[?6n");
    assert!(resp.is_empty());
    // A `?`-DSR the client does not answer also passes through — including
    // the colour-scheme report (996) and 998, which SwiftTerm leaves silent.
    for query in [&b"\x1b[?53n"[..], b"\x1b[?996n", b"\x1b[?998n"] {
        let (out, resp) = run(query);
        assert_eq!(out, query);
        assert!(resp.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Answers the client does not give are not invented.
// ---------------------------------------------------------------------------

#[test]
fn queries_the_client_leaves_unanswered_pass_through() {
    for query in [
        &b"\x1b[:c"[..], // leading `:` aborts the client's CSI parse
        b"\x1b[::c",
        b"\x1b[=c", // tertiary DA: SwiftTerm has no `=` branch
        b"\x1b[=0c",
        b"\x1b]4;1;?\x07", // palette: not the xterm one, not ours to report
        b"\x1b]4;1;?;9;?\x1b\\",
        b"\x1b]4;1;rgb:ff/00/00\x07",
    ] {
        let (out, resp) = run(query);
        assert_eq!(out, query, "must reach the client: {query:?}");
        assert!(resp.is_empty(), "must not be answered: {query:?}");
    }
}

#[test]
fn more_than_24_parameters_is_neither_answered_nor_tracked() {
    // SwiftTerm skips dispatch past 24 groups, so `CSI 0;…;0 c` (25 groups)
    // is not a DA request there, and a 25-group push does not push.
    let da = format!("\x1b[{}c", vec!["0"; 25].join(";"));
    let (out, resp) = run(da.as_bytes());
    assert_eq!(out, da.as_bytes());
    assert!(resp.is_empty());
    let mut p = CapabilityProxy::new();
    let push = format!("\x1b[>1{}u", ";0".repeat(24));
    passthrough(&mut p, push.as_bytes());
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?0u");
    // 24 groups is still fine.
    let da24 = format!("\x1b[{}c", vec!["0"; 24].join(";"));
    let (_, resp) = run(da24.as_bytes());
    assert_eq!(resp, b"\x1b[?65;4;1;2;6;21;22;17;28c");
}

// ---------------------------------------------------------------------------
// Resync — the client starts fresh on attach and resets on a gap; the proxy
// is brought back in line by replaying exactly what the client is fed.
// ---------------------------------------------------------------------------

#[test]
fn resync_resets_then_observes_the_replay_only() {
    let mut p = CapabilityProxy::new();
    passthrough(&mut p, b"\x1b[>5u\x1b[?1049h\x1b[>3u");
    // A fresh client fed nothing: everything back to zero, normal screen.
    p.resync(b"");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?0u");
    // A client fed a replay that pushes: the proxy follows the replay.
    p.resync(b"output\x1b[>1u more");
    let (_, resp) = p.process(b"\x1b[?u");
    assert_eq!(resp, b"\x1b[?1u");
}

#[test]
fn resync_answers_nothing_and_counts_nothing() {
    let mut p = CapabilityProxy::new();
    let before = p.answered();
    p.resync(b"\x1b[c\x1b[?u\x1b]10;?\x07");
    assert_eq!(
        p.answered(),
        before,
        "replayed queries were answered already"
    );
}

// ---------------------------------------------------------------------------
// OSC colour queries — 10/11/12 with arbitrary `?` groups.
// ---------------------------------------------------------------------------

#[test]
fn answers_cursor_colour_query() {
    // OSC 12: SwiftTerm answers the cursor colour, which defaults to the
    // foreground — the runtime's declared light-grey.
    let (out, resp) = run(b"\x1b]12;?\x07");
    assert!(out.is_empty());
    assert_eq!(resp, b"\x1b]12;rgb:c7c7/c7c7/c7c7\x07");
}

#[test]
fn multi_group_osc_query_answers_each_colour() {
    // `OSC 10 ; ? ; ?` is a query for foreground AND background: SwiftTerm
    // replies once per `?` group — two separate answers.
    let (out, resp) = run(b"\x1b]10;?;?\x1b\\");
    assert!(out.is_empty());
    assert_eq!(
        resp,
        b"\x1b]10;rgb:c7c7/c7c7/c7c7\x1b\\\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"
    );
    // From OSC 11 the same group form reaches background + cursor.
    let (_, resp) = run(b"\x1b]11;?;?\x07");
    assert_eq!(
        resp,
        b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07\x1b]12;rgb:c7c7/c7c7/c7c7\x07"
    );
}

#[test]
fn empty_osc_groups_do_not_shift_the_target() {
    // Swift's `split` omits empty groups: `10;?;;?` is fg then bg.
    let (_, resp) = run(b"\x1b]10;?;;?\x07");
    assert_eq!(
        resp,
        b"\x1b]10;rgb:c7c7/c7c7/c7c7\x07\x1b]11;rgb:1e1e/1e1e/1e1e\x07"
    );
    let (_, resp) = run(b"\x1b]11;;?\x07");
    assert_eq!(resp, b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07");
}

#[test]
fn osc_question_mark_prefixed_group_is_a_query() {
    // SwiftTerm tests `group.first == '?'`, so `?junk` still queries.
    let (_, resp) = run(b"\x1b]10;?junk\x07");
    assert_eq!(resp, b"\x1b]10;rgb:c7c7/c7c7/c7c7\x07");
}

#[test]
fn osc_colour_sets_pass_through_including_mixed_groups() {
    // A colour value is a set the client must apply — never consumed here.
    for seq in [
        &b"\x1b]10;rgb:ff/00/00\x07"[..],
        b"\x1b]10;#ff0000\x1b\\",
        b"\x1b]10;?;rgb:ff/00/00\x07", // query + set: the set must reach the client
        b"\x1b]12;#abc\x07",
    ] {
        let (out, resp) = run(seq);
        assert_eq!(out, seq, "a set must not be consumed: {seq:?}");
        assert!(resp.is_empty());
    }
}

#[test]
fn osc_colour_specs_match_the_clients_case_sensitivity() {
    // `parseColor` only accepts a lowercase `rgb:` — an uppercase form is a
    // client-side no-op, not a set, so consuming it loses nothing.
    let (out, resp) = run(b"\x1b]10;RGB:FF/00/00\x07");
    assert!(out.is_empty());
    assert!(resp.is_empty());
}

// ---------------------------------------------------------------------------
// Chunk scope — a query split across two reads is not recognised (and, per
// the module docs, gets no answer at all: the client's echo is filtered).
// ---------------------------------------------------------------------------

#[test]
fn a_query_split_across_process_calls_passes_both_halves() {
    let mut p = CapabilityProxy::new();
    let (out, resp) = p.process(b"prompt \x1b[5");
    assert_eq!(out.as_ref(), b"prompt \x1b[5");
    assert!(resp.is_empty());
    let (out, resp) = p.process(b"n more output");
    assert_eq!(out.as_ref(), b"n more output");
    assert!(resp.is_empty(), "a split query is never answered");
}

// ---------------------------------------------------------------------------
// Filter mirror — every reply shape the proxy emits must also be stripped
// client→PTY by TermFilter, or a client echo of a split/replayed query leaks
// a duplicate answer into the child.
// ---------------------------------------------------------------------------

#[test]
fn every_answer_shape_the_proxy_emits_is_filtered_on_the_way_back() {
    use openab_pty::termfilter::TermFilter;
    let mut filter = TermFilter::new();
    for reply in [
        &b"\x1b[?65;4;1;2;6;21;22;17;28c"[..], // primary DA
        b"\x1b[>65;20;1c",                     // secondary DA
        b"\x1b[0n",                            // DSR ready
        b"\x1b[?10n",                          // DSR ?-family …
        b"\x1b[?27;1;0;0n",
        b"\x1b[0*{",                         // macro space report
        b"\x1bP7!~0000\x1b\\",               // macro checksum report
        b"\x1b[?5u",                         // kitty flag report
        b"\x1b]10;rgb:c7c7/c7c7/c7c7\x07",   // OSC fg colour
        b"\x1b]12;rgb:c7c7/c7c7/c7c7\x1b\\", // OSC cursor colour
    ] {
        assert!(
            filter.filter(reply).is_empty(),
            "a proxied reply shape must be filtered: {reply:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Unproxied reply passthrough (#53) — queries the proxy leaves to the client
// (OSC 4 palette, XTVERSION, tertiary DA, DECRQSS, XTGETTCAP, and CPR) must
// not have their client replies eaten by TermFilter on the way back to the child.
// ---------------------------------------------------------------------------

#[test]
fn client_replies_to_unproxied_queries_reach_child() {
    use openab_pty::termfilter::TermFilter;
    let mut filter = TermFilter::new();
    for reply in [
        &b"\x1b[24;80R"[..],                    // CPR
        b"\x1b[?24;80;1R",                      // DECXCPR
        b"\x1b]4;1;rgb:cd00/0000/0000\x07",     // OSC 4 palette reply (BEL)
        b"\x1b]4;255;rgb:ffff/ffff/ffff\x1b\\", // OSC 4 palette reply (ST)
        b"\x1bP>|SwiftTerm 1.19\x1b\\",         // XTVERSION
        b"\x1bP>|xterm.js(5.3.0)\x1b\\",
        b"\x1bP!|7E565445\x1b\\",         // tertiary DA
        b"\x1bP1$r0;1m\x1b\\",            // DECRQSS
        b"\x1bP1+r544e=787465726d\x1b\\", // XTGETTCAP
    ] {
        assert_eq!(
            filter.filter(reply).as_ref(),
            reply,
            "unproxied reply shape must pass to the child: {reply:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Interleaving with real output — answers concatenate in query order.
// ---------------------------------------------------------------------------

#[test]
fn the_new_queries_strip_out_of_a_mixed_burst() {
    let input = b"\x1b[=c\x1b[?15n\x1b]12;?\x07shell$ ";
    let (out, resp) = run(input);
    // Tertiary DA is not answered, so it stays in the stream for the client.
    assert_eq!(out, b"\x1b[=cshell$ ");
    assert_eq!(resp, b"\x1b[?10n\x1b]12;rgb:c7c7/c7c7/c7c7\x07");
}
