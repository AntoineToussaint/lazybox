//! Daemon-side scan of terminal OUTPUT for the `/` search's `agent:` /
//! `said:` qualifiers (#1780).
//!
//! Stage 1 (#1774) searched the prompt history — what the agent was
//! *asked* — which the client already holds. What the agent *said back*
//! is only here: it lives in the per-terminal replay rings
//! ([`crate::pty::REPLAY_RING_BYTES`]), and the client keeps a 4 KiB
//! rolling window per terminal for agent-state detection, far too shallow
//! to answer "an hour ago".
//!
//! Two properties make this more than plumbing:
//!
//! - **The rings are repaint-laden.** An agent TUI redraws its whole box
//!   continuously, so a raw byte window is mostly duplicated frames. The
//!   scan therefore returns deduplicated matching *lines*, not a window —
//!   and reconstructs those lines from cursor movement, because a
//!   full-screen repaint positions each row with a CSI rather than a
//!   newline.
//! - **Cost must be a function of terminal count, not of how chatty an
//!   agent has been.** Only the newest [`SCAN_TAIL_BYTES`] of each
//!   terminal is scanned, and each workspace contributes at most
//!   [`MATCH_CORPUS_BYTES`], so a query's price is bounded before it is
//!   issued.

use futures::{StreamExt, stream};
use lazybox_core::SessionKey;
use lazybox_ipc::{Event, MAX_AGENT_OUTPUT_NEEDLE_BYTES, MAX_AGENT_OUTPUT_NEEDLES};
use std::collections::BTreeMap;

use crate::ServerConfig;
use crate::spawn_handler::SNAPSHOT_CONCURRENCY;

/// Newest bytes of each terminal's ring the scan reads.
///
/// The ring holds up to [`crate::pty::REPLAY_RING_BYTES`] (2 MiB), but a
/// repainting agent spends that budget fast, and a query is overwhelmingly
/// about recent work. 256 KiB is an eighth of the ring — deep enough to
/// cover the current turn and several before it once repaint churn is
/// collapsed, shallow enough that forty live terminals cost megabytes, not
/// a hundred of them.
pub const SCAN_TAIL_BYTES: usize = 256 * 1024;

/// Bytes of matched text one workspace contributes to the reply.
///
/// Sized so EVERY needle's first hit always fits (the `const` assert
/// below pins that), because the client ANDs the `agent:` terms against
/// this text: a needle whose only evidence was trimmed away here reads as
/// "not in this workspace" and drops a row that genuinely matched.
pub const MATCH_CORPUS_BYTES: usize = 8 * 1024;

/// Distinct matching lines kept **per needle**, not per workspace.
///
/// A single global cap starved later needles: matching is first-come
/// across all needles, so a terminal with a repeating `error` line filled
/// the whole budget and a `deadlock` line further along never made it
/// back. The client, requiring both `agent:error` and `agent:deadlock`,
/// then found no evidence for the second and excluded a workspace whose
/// output contained both — a silent miss with nothing on screen to
/// explain it. Per-needle buckets give every term its own room.
const MAX_MATCH_LINES_PER_NEEDLE: usize = 4;

/// Longest single matched line carried back. A pasted log line or a
/// wrapped-then-rejoined repaint row can be enormous; the needle plus its
/// surroundings is all the excerpt can show.
const MAX_MATCH_LINE_BYTES: usize = 512;

/// The corpus must hold one full-length line for every needle, or the
/// trimming in [`fold_matches`] could still drop a needle's only evidence
/// and reintroduce the silent miss the per-needle buckets exist to stop.
const _: () = assert!(
    MATCH_CORPUS_BYTES >= MAX_AGENT_OUTPUT_NEEDLES * (MAX_MATCH_LINE_BYTES + 1),
    "MATCH_CORPUS_BYTES must fit one line per needle"
);

/// `Command::SearchAgentOutput` — scan every live agent terminal's ring
/// for `needles` and reply to the asking connection with the matching
/// lines per workspace.
///
/// The ring snapshots are taken on the async side (they are memory copies
/// under the PTY's own lock) and the scan itself runs on `spawn_blocking`:
/// stripping and matching megabytes is CPU work, and the daemon's runtime
/// workers also carry every terminal's output pump.
///
/// An empty reply is still sent. It is the answer that clears a previous
/// query's rows, and a client that never hears back cannot tell "nothing
/// matched" from "still scanning".
pub async fn handle_search_agent_output(
    config: &ServerConfig,
    tx: &lazybox_ipc::EventSender,
    request_id: u64,
    needles: Vec<String>,
) {
    let needles = normalize_needles(needles);
    if needles.is_empty() {
        let _ = tx.send(Event::AgentOutputMatches {
            request_id,
            entries: Vec::new(),
        });
        return;
    }

    // Bounded fan-out, not a sequential loop: the deadline below is
    // per-snapshot, so N wedged terminals cost N × the deadline in series —
    // the exact pathology `SNAPSHOT_CONCURRENCY` was introduced for on the
    // Subscribe path. Unlimited fan-out would instead stampede the ring
    // locks on a large installation, so this borrows both bounds.
    let targets = config.terminal.agent_terminal_backends().await;
    let tails: Vec<(SessionKey, Vec<u8>)> = stream::iter(targets)
        .map(|(session_key, backend_key)| async move {
            // Same deadline the Subscribe path puts on a snapshot, for the
            // same reason: a pump wedged on the per-PTY ring mutex must cost
            // this query one terminal's worth of history, not the whole
            // reply.
            match tokio::time::timeout(
                crate::spawn_handler::SNAPSHOT_PER_SESSION_TIMEOUT,
                config.backend.snapshot(&backend_key),
            )
            .await
            {
                Ok(Ok(snapshot)) => {
                    let tail = tail_from_line_boundary(&snapshot.replay, SCAN_TAIL_BYTES).to_vec();
                    (!tail.is_empty()).then_some((session_key, tail))
                }
                Ok(Err(e)) => {
                    tracing::debug!(key = %backend_key, "agent output scan: snapshot failed: {e}");
                    None
                }
                Err(_) => {
                    tracing::debug!(key = %backend_key, "agent output scan: snapshot timed out");
                    None
                }
            }
        })
        .buffered(SNAPSHOT_CONCURRENCY)
        .filter_map(|tail| async move { tail })
        .collect()
        .await;

    let entries = match tokio::task::spawn_blocking(move || fold_matches(tails, &needles)).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("agent output scan panicked: {e}");
            Vec::new()
        }
    };
    let _ = tx.send(Event::AgentOutputMatches {
        request_id,
        entries,
    });
}

/// Clamp what a client asked to scan for: drop blanks, fold case (the
/// client's query normalization already lowercased, so this only makes the
/// daemon independent of that), truncate an over-long needle on a char
/// boundary, and keep at most [`MAX_AGENT_OUTPUT_NEEDLES`].
fn normalize_needles(needles: Vec<String>) -> Vec<String> {
    needles
        .into_iter()
        .map(|needle| needle.to_lowercase())
        .filter(|needle| !needle.trim().is_empty())
        .map(|mut needle| {
            if needle.len() > MAX_AGENT_OUTPUT_NEEDLE_BYTES {
                let mut end = MAX_AGENT_OUTPUT_NEEDLE_BYTES;
                while end > 0 && !needle.is_char_boundary(end) {
                    end -= 1;
                }
                needle.truncate(end);
            }
            needle
        })
        .filter(|needle| !needle.is_empty())
        .take(MAX_AGENT_OUTPUT_NEEDLES)
        .collect()
}

/// Scan every terminal's tail and fold the per-terminal results onto their
/// workspaces — a workspace with two agent tabs contributes both, since the
/// search filters workspaces rather than tabs.
fn fold_matches(tails: Vec<(SessionKey, Vec<u8>)>, needles: &[String]) -> Vec<(String, String)> {
    // Buckets are merged across a workspace's terminals BEFORE the
    // round-robin, not after. Interleaving per terminal and concatenating
    // would let a first tab that matches many needles with long lines spend
    // the whole corpus, dropping a second tab's only evidence for a needle
    // the first never matched — the per-terminal shape of the very starvation
    // the buckets exist to prevent.
    let mut per_session: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    for (session_key, tail) in tails {
        let buckets = scan_buckets(&tail, needles);
        if buckets.iter().all(Vec::is_empty) {
            continue;
        }
        let merged = per_session
            .entry(session_key.to_string())
            .or_insert_with(|| vec![Vec::new(); needles.len()]);
        for (slot, hits) in buckets.into_iter().enumerate() {
            for hit in hits {
                if merged[slot].len() < MAX_MATCH_LINES_PER_NEEDLE && !merged[slot].contains(&hit) {
                    merged[slot].push(hit);
                }
            }
        }
    }
    per_session
        .into_iter()
        .filter_map(|(key, buckets)| {
            let mut corpus = String::new();
            for line in round_robin(&buckets) {
                if corpus.len() + line.len() + 1 > MATCH_CORPUS_BYTES {
                    break;
                }
                corpus.push_str(&line);
                corpus.push('\n');
            }
            (!corpus.is_empty()).then_some((key, corpus))
        })
        .collect()
}

/// Flatten per-needle buckets newest-priority-first: every needle's first
/// hit, then every needle's second, and so on, deduplicated. The order is
/// what makes the byte trim above safe — the first round is one line per
/// needle, which [`MATCH_CORPUS_BYTES`] is asserted to hold.
fn round_robin(buckets: &[Vec<String>]) -> Vec<String> {
    let mut ordered: Vec<String> = Vec::new();
    for round in 0..MAX_MATCH_LINES_PER_NEEDLE {
        for bucket in buckets {
            if let Some(hit) = bucket.get(round)
                && !ordered.contains(hit)
            {
                ordered.push(hit.clone());
            }
        }
    }
    ordered
}

/// The newest `max_bytes` of `bytes`, advanced to the first line boundary
/// so the scan never opens inside an evicted escape sequence or a
/// half-written UTF-8 glyph. Returns the whole slice when it already fits.
fn tail_from_line_boundary(bytes: &[u8], max_bytes: usize) -> &[u8] {
    if bytes.len() <= max_bytes {
        return bytes;
    }
    let start = bytes.len() - max_bytes;
    match bytes[start..].iter().position(|&b| b == b'\n') {
        Some(offset) => &bytes[start + offset + 1..],
        None => &bytes[start..],
    }
}

/// Matching, deduplicated lines of `bytes`, newline-joined — or `None`
/// when nothing matched.
///
/// Lines are reconstructed rather than split on `\n`, because that is not
/// where a repainting program puts its row boundaries: it positions the
/// cursor with a CSI and writes the row. So a ROW-changing sequence ends
/// the current line, while SGR colour, OSC titles and erases are dropped
/// in place — splitting on those too would tear `error: cannot borrow`
/// apart at whatever byte the syntax highlighter recoloured, and a
/// multi-word needle would then match nothing. Column moves within a row
/// become a single space, because that is what the columns they skip
/// render as; dropping them outright would fuse `Name` and `Value` into
/// `NameValue`.
///
/// Evidence is kept PER NEEDLE and returned round-robin — every needle's
/// first hit, then every needle's second, and so on. The client ANDs the
/// `agent:` terms against this text, so a needle with no evidence excludes
/// the workspace; first-come ordering under one global cap let a chatty
/// needle spend the whole budget and silently drop a row that matched
/// every term.
///
/// Deduplication runs over MATCHING lines only. A repaint re-emits the
/// same row dozens of times and every copy would otherwise fill the reply
/// — but a set over EVERY line would be a set the size of the scan window,
/// and the reply is capped at a few lines per needle anyway, so the dedup
/// runs over that handful instead.
pub fn scan_output(bytes: &[u8], needles: &[String]) -> Option<String> {
    let ordered = round_robin(&scan_buckets(bytes, needles));
    (!ordered.is_empty()).then(|| ordered.join("\n"))
}

/// [`scan_output`]'s per-needle buckets, before they are flattened — what
/// [`fold_matches`] merges across a workspace's terminals.
fn scan_buckets(bytes: &[u8], needles: &[String]) -> Vec<Vec<String>> {
    let mut buckets: Vec<Vec<String>> = vec![Vec::new(); needles.len()];
    let mut line: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let ended = match bytes[i] {
            0x1b => {
                let end = lazybox_agents::detect::skip_escape(bytes, i);
                let effect = escape_effect(&bytes[i..end]);
                match effect {
                    EscapeEffect::EndsRow => take_line(&mut line, needles, &mut buckets),
                    // A skipped column renders as blank, so it separates
                    // words exactly as a space does; the whitespace collapse
                    // in `take_line` folds a run of them back to one.
                    EscapeEffect::SkipsColumns => line.push(b' '),
                    EscapeEffect::Invisible => {}
                }
                i = end;
                matches!(effect, EscapeEffect::EndsRow)
            }
            b'\n' | b'\r' => {
                take_line(&mut line, needles, &mut buckets);
                i += 1;
                true
            }
            // Other C0 controls (BEL, backspace, the tabs a box-drawing
            // repaint sprays) are noise inside a line, not content.
            b if b < 0x20 && b != b'\t' => {
                i += 1;
                false
            }
            b => {
                line.push(b);
                i += 1;
                false
            }
        };
        if ended
            && buckets
                .iter()
                .all(|b| b.len() >= MAX_MATCH_LINES_PER_NEEDLE)
        {
            break;
        }
    }
    take_line(&mut line, needles, &mut buckets);
    buckets
}

/// What a stripped escape run does to the text being reconstructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeEffect {
    /// Content after it lands on a different row — a line boundary.
    EndsRow,
    /// The cursor moves within the SAME row, leaving blank columns behind.
    SkipsColumns,
    /// No effect on layout: colour, titles, device reports, erases.
    Invisible,
}

/// Classify a stripped escape run by what it does to row layout.
///
/// Only sequences that change the ROW end a line. The distinction matters
/// because `C` (CUF), `D` (CUB) and `G` (CHA) move the cursor *within* the
/// current row — programs pad columns with `\x1b[<n>C` because it is
/// shorter than spaces — so treating them as boundaries split one rendered
/// row into fragments and made every needle spanning the padding miss.
/// They skip columns instead, which reconstruct as a blank.
///
/// Erases (`J`, `K`) change no row either: `\x1b[2K` is emitted right
/// after the positioning sequence that already ended the line, and a
/// trailing `\x1b[K` clears leftovers on the row just written — breaking
/// there would split `foo\x1b[K` from the `bar` that lands beside it.
fn escape_effect(run: &[u8]) -> EscapeEffect {
    if run.first() != Some(&0x1b) || run.get(1) != Some(&b'[') {
        return EscapeEffect::Invisible;
    }
    match run.last() {
        // CUU / CUD / CNL / CPL / CUP / VPA / HVP change the row, and
        // IL / DL / SU / SD shift which row content lands on.
        Some(b'A' | b'B' | b'E' | b'F' | b'H' | b'L' | b'M' | b'S' | b'T' | b'd' | b'f') => {
            EscapeEffect::EndsRow
        }
        // CUF / CUB / CHA are column moves inside one row.
        Some(b'C' | b'D' | b'G') => EscapeEffect::SkipsColumns,
        _ => EscapeEffect::Invisible,
    }
}

/// Close the pending line: decode, collapse whitespace, and file it under
/// every needle it matches that still has room.
///
/// Per-needle buckets rather than one list: the client ANDs the `agent:`
/// terms, so a needle left without evidence excludes the workspace
/// outright. A line matching several needles counts for all of them.
fn take_line(line: &mut Vec<u8>, needles: &[String], buckets: &mut [Vec<String>]) {
    let raw = std::mem::take(line);
    if raw.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(&raw);
    let mut collapsed = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
    }
    if collapsed.is_empty() {
        return;
    }
    let folded = collapsed.to_lowercase();
    let wanted: Vec<usize> = needles
        .iter()
        .enumerate()
        .filter(|(slot, needle)| {
            folded.contains(needle.as_str())
                && buckets[*slot].len() < MAX_MATCH_LINES_PER_NEEDLE
                && !buckets[*slot].contains(&collapsed)
        })
        .map(|(slot, _)| slot)
        .collect();
    if wanted.is_empty() {
        return;
    }
    if collapsed.len() > MAX_MATCH_LINE_BYTES {
        let mut end = MAX_MATCH_LINE_BYTES;
        while end > 0 && !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        collapsed.truncate(end);
    }
    for slot in wanted {
        buckets[slot].push(collapsed.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needles(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }

    /// The motivating case: the agent said something, wrapped in the SGR
    /// colour its CLI paints errors with. Stripping has to leave the
    /// phrase contiguous — a break at the colour change would make every
    /// multi-word needle miss.
    #[test]
    fn ansi_escapes_do_not_defeat_a_match_on_the_text_they_wrap() {
        let output =
            b"\x1b[1;31merror[E0502]\x1b[0m: \x1b[1mcannot borrow\x1b[0m `self` as mutable\r\n";
        let hit = scan_output(output, &needles(&["cannot borrow"])).expect("the phrase matches");
        assert!(hit.contains("cannot borrow `self` as mutable"), "{hit:?}");
        assert!(!hit.contains('\x1b'), "escapes must not reach the client");

        // And the same phrase split by a colour change mid-word still
        // matches, because SGR does not end a line.
        let recoloured = b"\x1b[31mcannot \x1b[32mborrow\x1b[0m here";
        assert!(scan_output(recoloured, &needles(&["cannot borrow"])).is_some());
    }

    /// A cursor move IS a line boundary: a repainting TUI writes rows with
    /// CSI positioning and no newline at all, so two unrelated rows must
    /// not fuse into one line a needle spuriously spans.
    #[test]
    fn a_cursor_move_ends_the_line_but_a_colour_does_not() {
        let repaint = b"\x1b[1;1Hchecking the lexer\x1b[2;1Hbuilding the parser";
        let hit = scan_output(repaint, &needles(&["lexer building"]));
        assert!(
            hit.is_none(),
            "rows separated by a cursor move are distinct"
        );

        let hit = scan_output(repaint, &needles(&["building the parser"]))
            .expect("each row is searchable on its own");
        assert_eq!(hit, "building the parser");
    }

    /// Repaint churn is the reason this is a line scan and not a byte
    /// window: the same row arrives dozens of times and must be reported
    /// once.
    #[test]
    fn a_repeated_repaint_row_is_reported_once() {
        let mut stream = Vec::new();
        for _ in 0..50 {
            stream.extend_from_slice(b"\x1b[1;1H\x1b[2Kcompiling lazybox-server v0.1.0\r\n");
        }
        let hit = scan_output(&stream, &needles(&["lazybox-server"])).expect("matches");
        assert_eq!(hit, "compiling lazybox-server v0.1.0");
    }

    /// Regression for the silent miss a single global line cap produced: a
    /// chatty needle filled the whole budget and a later needle came back
    /// with no evidence, so the client's AND dropped a workspace whose
    /// output contained both terms.
    #[test]
    fn a_chatty_needle_cannot_starve_a_later_one() {
        let mut stream = Vec::new();
        // Far more `error` lines than any per-needle budget, all distinct so
        // dedup cannot collapse them.
        for n in 0..50 {
            stream.extend_from_slice(format!("error: variant {n} failed\n").as_bytes());
        }
        stream.extend_from_slice(b"thread 'main' hit a deadlock\n");

        let hit = scan_output(&stream, &needles(&["error", "deadlock"]))
            .expect("both needles have evidence");
        assert!(
            hit.lines().any(|l| l.contains("deadlock")),
            "the later needle must survive a chatty earlier one: {hit:?}"
        );
        assert!(hit.lines().any(|l| l.contains("error:")), "{hit:?}");

        // And the round-robin puts each needle's first hit up front, so the
        // per-workspace byte trim in `fold_matches` cannot drop one either.
        let entries = fold_matches(
            vec![(SessionKey::from("github:o/r#1"), stream)],
            &needles(&["error", "deadlock"]),
        );
        assert!(
            entries[0].1.contains("deadlock"),
            "the corpus trim must not drop a needle's only evidence: {:?}",
            entries[0].1
        );
    }

    /// The per-needle guarantee has to hold at WORKSPACE level, because that
    /// is what the client ANDs against. Folding terminal-by-terminal let a
    /// first tab that matches many needles with long lines spend the whole
    /// corpus and drop a second tab's only evidence for a needle the first
    /// never matched.
    #[test]
    fn a_second_tab_keeps_its_evidence_when_the_first_is_verbose() {
        let key = SessionKey::from("github:o/r#1");
        let filler = "x".repeat(MAX_MATCH_LINE_BYTES);
        let mut verbose = Vec::new();
        // The first tab matches seven of the eight needles, with maximal
        // lines, enough raw text to overrun the corpus on its own.
        for slot in 0..7 {
            for n in 0..MAX_MATCH_LINES_PER_NEEDLE {
                verbose.extend_from_slice(format!("term{slot} {n} {filler}\n").as_bytes());
            }
        }
        let quiet = b"only here: term7 appears\n".to_vec();

        let mut asked: Vec<String> = (0..8).map(|n| format!("term{n}")).collect();
        asked.truncate(MAX_AGENT_OUTPUT_NEEDLES);
        let entries = fold_matches(vec![(key.clone(), verbose), (key.clone(), quiet)], &asked);
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].1.contains("term7"),
            "the quiet tab's only evidence must survive a verbose sibling: {:?}",
            entries[0].1
        );
        assert!(entries[0].1.len() <= MATCH_CORPUS_BYTES);
    }

    /// `\x1b[<n>C` is a COLUMN move inside one row — programs pad with it
    /// because it is shorter than spaces. Treating it as a line boundary
    /// split one rendered row in two and made every needle spanning the
    /// padding miss.
    #[test]
    fn a_column_move_pads_the_row_instead_of_ending_it() {
        let padded = b"Name\x1b[6CValue is here";
        let hit = scan_output(padded, &needles(&["name value"]))
            .expect("the row reads as one line across the padding");
        assert_eq!(hit, "Name Value is here");

        // CHA (`G`) and CUB (`D`) are column moves too.
        assert!(scan_output(b"left\x1b[20Gright", &needles(&["left right"])).is_some());
        assert!(scan_output(b"left\x1b[2Dright", &needles(&["left right"])).is_some());

        // An erase clears leftovers on the row just written; it does not end
        // it, so content beside it stays on the same line.
        let erased = b"foo\x1b[Kbar";
        assert_eq!(
            scan_output(erased, &needles(&["foobar"])),
            Some("foobar".to_string())
        );

        // Row moves still end the line — the property the padding fix must
        // not cost us.
        assert!(scan_output(b"a\x1b[2;1Hb", &needles(&["a b"])).is_none());
    }

    /// Cost is bounded by the tail, not by how chatty the agent has been:
    /// a match older than [`SCAN_TAIL_BYTES`] is out of reach, and the
    /// scan never touches those bytes.
    #[test]
    fn only_the_tail_of_a_long_ring_is_scanned() {
        let mut stream = b"the oldest thing the agent said\n".to_vec();
        stream.resize(stream.len() + SCAN_TAIL_BYTES * 2, b'.');
        stream.extend_from_slice(b"\nthe newest thing the agent said\n");

        let tail = tail_from_line_boundary(&stream, SCAN_TAIL_BYTES);
        assert!(tail.len() <= SCAN_TAIL_BYTES);
        assert!(scan_output(tail, &needles(&["oldest thing"])).is_none());
        assert!(scan_output(tail, &needles(&["newest thing"])).is_some());
    }

    /// A tail cut mid-escape must not leak the sequence's payload into
    /// matched text: the cut is advanced to the next line boundary.
    #[test]
    fn a_tail_opens_on_a_line_boundary() {
        let mut stream = b"first line\n".to_vec();
        stream.extend_from_slice(b"\x1b[38;5;214msecond line\x1b[0m\n");
        let tail = tail_from_line_boundary(&stream, stream.len() - 5);
        assert!(tail.starts_with(b"\x1b[38"), "{:?}", &tail[..4]);
    }

    /// Matching is case-insensitive in the same direction as the client's
    /// qualifier, which lowercases the whole query before splitting terms.
    #[test]
    fn matching_folds_case_like_the_client_qualifier() {
        let hit = scan_output(
            b"Deadlock detected in the scheduler\n",
            &needles(&["deadlock"]),
        )
        .expect("a lowercase needle finds mixed-case output");
        assert_eq!(hit, "Deadlock detected in the scheduler");
    }

    /// The reply is bounded per workspace even when every line matches,
    /// and two terminals of one workspace fold onto a single entry.
    #[test]
    fn a_workspace_entry_is_bounded_and_folds_its_terminals() {
        let key = SessionKey::from("github:o/r#1");
        let mut chatty = Vec::new();
        for n in 0..500 {
            chatty.extend_from_slice(format!("parser note {n} padded out a bit\n").as_bytes());
        }
        let entries = fold_matches(
            vec![
                (key.clone(), chatty.clone()),
                (key.clone(), b"parser note from the second tab\n".to_vec()),
            ],
            &needles(&["parser"]),
        );
        assert_eq!(
            entries.len(),
            1,
            "one entry per workspace, not per terminal"
        );
        assert_eq!(entries[0].0, key.to_string());
        assert!(
            entries[0].1.len() <= MATCH_CORPUS_BYTES,
            "{} bytes",
            entries[0].1.len()
        );
        assert!(
            entries[0].1.lines().count()
                <= MAX_MATCH_LINES_PER_NEEDLE * needles(&["parser"]).len() * 2,
            "each terminal is capped per needle"
        );
    }

    /// A workspace with nothing to say is absent rather than present-and-
    /// empty: the client merges these into its corpus, and an empty string
    /// would shadow the prompt-history text already keyed there.
    #[test]
    fn a_terminal_with_no_match_contributes_no_entry() {
        assert!(
            fold_matches(
                vec![(SessionKey::from("github:o/r#1"), b"all quiet\n".to_vec())],
                &needles(&["parser"]),
            )
            .is_empty()
        );
    }

    #[test]
    fn needles_are_clamped_before_they_reach_the_scan() {
        let long = "x".repeat(MAX_AGENT_OUTPUT_NEEDLE_BYTES * 2);
        let mut raw: Vec<String> = vec!["  ".into(), String::new(), "Parser".into(), long];
        // Push past the cap so the `take` is what bounds the result, not the
        // input length — the assertion below would otherwise pass for any
        // cap at or above the number of needles supplied.
        raw.extend((0..MAX_AGENT_OUTPUT_NEEDLES).map(|n| format!("extra{n}")));
        let clamped = normalize_needles(raw);
        assert_eq!(clamped.len(), MAX_AGENT_OUTPUT_NEEDLES);
        assert_eq!(clamped[0], "parser", "needles fold case");
        assert_eq!(clamped[1].len(), MAX_AGENT_OUTPUT_NEEDLE_BYTES);
    }

    /// A truncated needle must stay valid UTF-8 — the byte cap can land
    /// mid-glyph on a pasted multi-byte phrase.
    #[test]
    fn a_truncated_needle_stays_on_a_char_boundary() {
        let needle = "é".repeat(MAX_AGENT_OUTPUT_NEEDLE_BYTES);
        let clamped = normalize_needles(vec![needle]);
        assert!(clamped[0].len() <= MAX_AGENT_OUTPUT_NEEDLE_BYTES);
        assert!(clamped[0].chars().all(|c| c == 'é'));
    }
}
