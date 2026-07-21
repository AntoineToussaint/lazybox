//! Shared interactive-terminal protocol for shell-hosted coding agents.
//!
//! Agent adapters declare a [`PtyProtocol`]; this module owns prompt
//! sanitization, terminal framing, submit intent, and readiness policy. The
//! daemon then executes the resulting [`EncodedPrompt`] through one serialized
//! paste/settle/submit transaction. Provider adapters never hand-roll escape
//! sequences or Enter behavior.

/// Shape of the prompt behind an `InputNeeded` reading: whether a bare
/// chooser keystroke (`1`-`9`, `y`, `n`, Esc) is a complete answer. The
/// daemon records this alongside the cached state so the optimistic
/// "user answered the prompt" flip only fires on prompts a single
/// keystroke can actually answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptShape {
    /// Permission gate / chooser / Y-N dialog — one keystroke answers.
    Chooser,
    /// Free-text elicitation — the answer is composed text plus Enter;
    /// a bare digit is just typing into the field.
    FreeText,
}

/// How a shell-hosted agent expects programmatic prompt input to be framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptFraming {
    /// Write the sanitized prompt directly. When submission is requested,
    /// append LF to the same write.
    Line,
    /// Wrap the sanitized prompt in explicit bracketed-paste markers and,
    /// when submission is requested, send CR as a later independent write.
    BracketedPaste,
}

/// Whether initial prompt injection may use the first-output/settle fallback
/// or must wait until the adapter positively recognizes its composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessPolicy {
    /// The adapter has no authoritative composer detector. Injection may use
    /// the bounded first-output/settle fallback.
    BestEffort,
    /// Never inject until [`crate::Agent::detect_ready_for_prompt`] positively
    /// identifies a safe composer. Prevents a work prompt from being eaten by
    /// a startup trust or permission dialog.
    Required,
}

/// Caller intent for an encoded prompt interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptIntent {
    /// Commit the prompt and start the agent turn.
    Submit,
    /// Leave the prompt in the composer for the user to edit.
    Compose,
}

/// Complete write sequence for one prompt interaction. Keeping the initial
/// write and optional submit write in one value prevents adapters from
/// accidentally mixing an inline newline with a second Enter or from making
/// compose-only recall submit on line-oriented terminals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPrompt {
    initial_write: Vec<u8>,
    submit_write: Option<Vec<u8>>,
    echo_probes: Vec<String>,
}

impl EncodedPrompt {
    /// Number of bytes in the first terminal write, for bounded diagnostics.
    pub fn initial_write_len(&self) -> usize {
        self.initial_write.len()
    }

    /// Compact probes the paste-settle gate matches against post-paste output
    /// to recognize the composer echoing the paste (see
    /// [`crate::detect::paste_echo_observed`]). Empty for line-oriented
    /// framing, which has no settle gate.
    pub fn echo_probes(&self) -> &[String] {
        &self.echo_probes
    }

    /// Consume the sequence into the exact writes the server must perform in
    /// order. When the second item is `Some`, the server waits for the first
    /// write's repaint to settle before sending it.
    pub fn into_writes(self) -> (Vec<u8>, Option<Vec<u8>>) {
        (self.initial_write, self.submit_write)
    }
}

/// Declarative contract between an agent's interactive composer and the
/// shared shell/PTY wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyProtocol {
    framing: PromptFraming,
    readiness: ReadinessPolicy,
}

impl PtyProtocol {
    /// Default for simple CLIs that accept ordinary line input and do not
    /// expose a reliable visual composer-ready marker.
    pub const LINE_ORIENTED: Self = Self::new(PromptFraming::Line, ReadinessPolicy::BestEffort);

    /// Shared full-screen composer behavior used by Claude and Codex.
    pub const GUARDED_COMPOSER: Self =
        Self::new(PromptFraming::BracketedPaste, ReadinessPolicy::Required);

    /// Build a protocol for an agent whose framing/readiness combination is
    /// not covered by the shared constants.
    pub const fn new(framing: PromptFraming, readiness: ReadinessPolicy) -> Self {
        Self { framing, readiness }
    }

    /// Framing used for prompt text and its submit keystroke.
    pub const fn framing(self) -> PromptFraming {
        self.framing
    }

    /// Confidence policy used by spawn-time composer readiness gating.
    pub const fn readiness(self) -> ReadinessPolicy {
        self.readiness
    }

    /// Encode untrusted prompt text according to this PTY protocol.
    ///
    /// Raw ESC bytes are always removed before framing. PR/issue text is
    /// third-party-authored; without this guard an embedded `ESC[201~` could
    /// end bracketed paste early and turn the remainder into live keystrokes.
    /// A leading blank-line prefix and its padding are omitted so delivered
    /// text starts on the composer's prompt row. A prompt that begins directly
    /// with indented content keeps that indentation.
    pub fn encode_prompt(self, prompt: &str, intent: PromptIntent) -> EncodedPrompt {
        let sanitized = trim_leading_blank_lines(prompt)
            .bytes()
            .filter(|&byte| byte != 0x1b);
        match self.framing {
            PromptFraming::Line => {
                let submit = intent == PromptIntent::Submit;
                let mut initial_write = Vec::with_capacity(prompt.len() + usize::from(submit));
                initial_write.extend(sanitized);
                if submit {
                    initial_write.push(b'\n');
                }
                EncodedPrompt {
                    initial_write,
                    submit_write: None,
                    echo_probes: Vec::new(),
                }
            }
            PromptFraming::BracketedPaste => {
                let mut initial_write = Vec::with_capacity(prompt.len() + 12);
                initial_write.extend_from_slice(b"\x1b[200~");
                initial_write.extend(sanitized);
                initial_write.extend_from_slice(b"\x1b[201~");
                // Probes the paste-settle gate uses to spot the composer
                // echoing this paste: a compact fragment of the prompt
                // itself, plus the collapsed-paste placeholder chrome.
                // Derived from the text as actually framed — post
                // blank-line trim — so the probe always describes bytes the
                // composer can echo.
                let mut echo_probes: Vec<String> =
                    crate::detect::paste_echo_probe(trim_leading_blank_lines(prompt))
                        .into_iter()
                        .collect();
                echo_probes.extend(
                    crate::detect::PASTE_PLACEHOLDER_PROBES
                        .iter()
                        .map(|p| (*p).to_string()),
                );
                EncodedPrompt {
                    initial_write,
                    submit_write: (intent == PromptIntent::Submit).then(|| vec![b'\r']),
                    echo_probes,
                }
            }
        }
    }

    /// Whether injection must wait for positive composer-ready detection.
    pub const fn requires_ready(self) -> bool {
        matches!(self.readiness, ReadinessPolicy::Required)
    }
}

/// Remove a leading blank-line prefix while preserving indentation when the
/// prompt begins directly with content. Raw escape bytes are ignored while
/// examining the prefix because [`PtyProtocol::encode_prompt`] removes them
/// before delivery.
pub fn trim_leading_blank_lines(prompt: &str) -> &str {
    let mut prefix_end = 0;
    let mut saw_line_break = false;
    for (index, c) in prompt.char_indices() {
        if c == '\x1b' || c.is_whitespace() {
            prefix_end = index + c.len_utf8();
            saw_line_break |= matches!(c, '\r' | '\n');
        } else {
            break;
        }
    }
    if saw_line_break {
        &prompt[prefix_end..]
    } else {
        prompt
    }
}

impl Default for PtyProtocol {
    fn default() -> Self {
        Self::LINE_ORIENTED
    }
}
