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

use lazybox_core::SessionKey;
use lazybox_ipc::{Event, MAX_AGENT_OUTPUT_NEEDLE_BYTES, MAX_AGENT_OUTPUT_NEEDLES};
use std::collections::BTreeMap;

use crate::ServerConfig;

/// Newest bytes of each terminal's ring the scan reads.
///
/// The ring holds up to [`crate::pty::REPLAY_RING_BYTES`] (2 MiB), but a
/// repainting agent spends that budget fast, and a query is overwhelmingly
/// about recent work. 256 KiB is an eighth of the ring — deep enough to
/// cover the current turn and several before it once repaint churn is
/// collapsed, shallow enough that forty live terminals cost megabytes, not
/// a hundred of them.
pub const SCAN_TAIL_BYTES: usize = 256 * 1024;

/// Bytes of matched text one workspace contributes to the reply. The
/// client wants enough to build a ~48-char excerpt and to satisfy the
/// other `agent:` terms of an AND-ed query; it has no use for a transcript.
pub const MATCH_CORPUS_BYTES: usize = 2 * 1024;

/// Distinct matching lines kept per workspace. Past a handful, extra hits
/// say nothing new about *which* workspace this is — and they are what a
/// repaint produces most of.
const MAX_MATCH_LINES: usize = 8;

/// Longest single matched line carried back. A pasted log line or a
/// wrapped-then-rejoined repaint row can be enormous; the needle plus its
/// surroundings is all the excerpt can show.
const MAX_MATCH_LINE_BYTES: usize = 512;

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

    let mut tails: Vec<(SessionKey, Vec<u8>)> = Vec::new();
    for (session_key, backend_key) in config.terminal.agent_terminal_backends().await {
        // Same deadline the Subscribe path puts on a snapshot, for the same
        // reason: a pump wedged on the per-PTY ring mutex must cost this
        // query one terminal's worth of history, not the whole reply.
        match tokio::time::timeout(
            crate::spawn_handler::SNAPSHOT_PER_SESSION_TIMEOUT,
            config.backend.snapshot(&backend_key),
        )
        .await
        {
            Ok(Ok(snapshot)) => {
                let tail = tail_from_line_boundary(&snapshot.replay, SCAN_TAIL_BYTES).to_vec();
                if !tail.is_empty() {
                    tails.push((session_key, tail));
                }
            }
            Ok(Err(e)) => {
                tracing::debug!(key = %backend_key, "agent output scan: snapshot failed: {e}");
            }
            Err(_) => {
                tracing::debug!(key = %backend_key, "agent output scan: snapshot timed out");
            }
        }
    }

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
    let mut per_session: BTreeMap<String, String> = BTreeMap::new();
    for (session_key, tail) in tails {
        let Some(matched) = scan_output(&tail, needles) else {
            continue;
        };
        let corpus = per_session.entry(session_key.to_string()).or_default();
        for line in matched.lines() {
            if corpus.len() + line.len() + 1 > MATCH_CORPUS_BYTES {
                break;
            }
            corpus.push_str(line);
            corpus.push('\n');
        }
    }
    per_session.into_iter().collect()
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
/// cursor with a CSI and writes the row. So a cursor-move or erase
/// sequence ends the current line, while SGR colour and OSC title runs are
/// dropped in place — splitting on those too would tear `error: cannot
/// borrow` apart at whatever byte the syntax highlighter recoloured, and a
/// multi-word needle would then match nothing.
///
/// Deduplication runs over MATCHING lines only. A repaint re-emits the
/// same row dozens of times and every copy would otherwise fill the reply
/// — but a set over EVERY line would be a set the size of the scan window,
/// and the reply is capped at a handful of lines anyway, so the dedup runs
/// over that handful instead.
pub fn scan_output(bytes: &[u8], needles: &[String]) -> Option<String> {
    let mut matches: Vec<String> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let ended = match bytes[i] {
            0x1b => {
                let end = lazybox_agents::detect::skip_escape(bytes, i);
                let ends_line = breaks_line(&bytes[i..end]);
                if ends_line {
                    take_line(&mut line, needles, &mut matches);
                }
                i = end;
                ends_line
            }
            b'\n' | b'\r' => {
                take_line(&mut line, needles, &mut matches);
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
        if ended && matches.len() >= MAX_MATCH_LINES {
            break;
        }
    }
    take_line(&mut line, needles, &mut matches);
    (!matches.is_empty()).then(|| matches.join("\n"))
}

/// Whether an escape run ends the current line. True for the CSI cursor
/// moves, erases and scrolls a repaint lays rows out with; false for SGR
/// (`m`), device reports, OSC / DCS strings and charset designators, which
/// occur *within* a row.
fn breaks_line(run: &[u8]) -> bool {
    if run.first() != Some(&0x1b) || run.get(1) != Some(&b'[') {
        return false;
    }
    matches!(
        run.last(),
        Some(
            b'A' | b'B'
                | b'C'
                | b'D'
                | b'E'
                | b'F'
                | b'G'
                | b'H'
                | b'J'
                | b'K'
                | b'L'
                | b'M'
                | b'S'
                | b'T'
                | b'd'
                | b'f'
        )
    )
}

/// Close the pending line: decode, collapse whitespace, and keep it when
/// it matches a needle and is not one the caller already has.
fn take_line(line: &mut Vec<u8>, needles: &[String], matches: &mut Vec<String>) {
    let raw = std::mem::take(line);
    if raw.is_empty() || matches.len() >= MAX_MATCH_LINES {
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
    if !needles
        .iter()
        .any(|needle| folded.contains(needle.as_str()))
    {
        return;
    }
    if collapsed.len() > MAX_MATCH_LINE_BYTES {
        let mut end = MAX_MATCH_LINE_BYTES;
        while end > 0 && !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        collapsed.truncate(end);
    }
    if !matches.contains(&collapsed) {
        matches.push(collapsed);
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
            entries[0].1.lines().count() <= MAX_MATCH_LINES * 2,
            "each terminal is capped at MAX_MATCH_LINES"
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
        let clamped = normalize_needles(vec![
            "  ".into(),
            String::new(),
            "Parser".into(),
            long,
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
        ]);
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
