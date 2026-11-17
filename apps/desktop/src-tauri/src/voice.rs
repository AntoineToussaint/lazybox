//! Voice (VOICE.md): dictation with the macOS Speech framework, on-device,
//! and read-aloud with AVSpeechSynthesizer.
//!
//! The page never touches the microphone. It asks the host to start a
//! recognition; the host opens the microphone (AVAudioEngine) only for that
//! recognition, streams partial and final text back to the calling webview as
//! `voice-event` events, and closes it when the recognition stops. No audio is
//! written anywhere. Recognition is on-device unless the person allowed
//! Apple's server for a language their Mac cannot recognize locally.
//!
//! Every Objective-C object lives on the main thread (`ACTIVE` and `SYNTH`
//! are thread-locals there); commands hop to it with `run_on_main_thread`.
//! The pure layer (permission states, language lists, levels, rates) is
//! platform-independent and unit-tested.
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tauri::{AppHandle, Webview};

/// An authorization the system holds for this app, for the microphone or for
/// speech recognition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    Granted,
    Denied,
    Restricted,
    NotDetermined,
    Unsupported,
}

impl Permission {
    /// `SFSpeechRecognizerAuthorizationStatus`: not determined 0, denied 1,
    /// restricted 2, authorized 3.
    pub fn from_speech(raw: isize) -> Self {
        match raw {
            0 => Self::NotDetermined,
            1 => Self::Denied,
            2 => Self::Restricted,
            3 => Self::Granted,
            _ => Self::Unsupported,
        }
    }
    /// `AVAuthorizationStatus` orders its values differently: not determined
    /// 0, restricted 1, denied 2, authorized 3.
    pub fn from_capture(raw: isize) -> Self {
        match raw {
            0 => Self::NotDetermined,
            1 => Self::Restricted,
            2 => Self::Denied,
            3 => Self::Granted,
            _ => Self::Unsupported,
        }
    }
}

/// What the Settings page and the dictation pill show before a recognition.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// The recognizer for the asked language exists and is ready.
    pub available: bool,
    /// That language is recognized on this Mac, without a server.
    pub on_device: bool,
    pub microphone: Permission,
    pub speech: Permission,
    /// Why it is not available, as a sentence; empty when it is.
    pub reason: String,
    /// The system's first preferred language (BCP 47), what "Automatic" uses.
    pub language: String,
}

impl Status {
    /// Where voice does not exist (the desktop app off macOS).
    #[cfg(any(test, not(target_os = "macos")))]
    pub fn unsupported(reason: &str) -> Self {
        Self {
            available: false,
            on_device: false,
            microphone: Permission::Unsupported,
            speech: Permission::Unsupported,
            reason: reason.into(),
            language: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Language {
    /// A BCP 47 tag: `en-US`, `fr-FR`, `zh-Hans-CN`.
    pub code: String,
    /// Its name in the person's language: "French (France)".
    pub name: String,
    pub on_device: bool,
}

/// A locale identifier as the Speech framework or the page names it
/// (`en_US`, `fr-FR`), as a BCP 47 tag; None for anything that is not one.
/// Only letters and digits in short subtags pass, so a value from the page
/// can never be more than a locale name.
pub fn bcp47(identifier: &str) -> Option<String> {
    let parts: Vec<&str> = identifier.split(['_', '-']).collect();
    let first = parts.first()?;
    if !(2..=3).contains(&first.len()) || !first.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    if parts.len() > 4
        || parts[1..]
            .iter()
            .any(|p| p.is_empty() || p.len() > 8 || !p.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        return None;
    }
    Some(
        parts
            .iter()
            .enumerate()
            .map(|(i, p)| match (i, p.len()) {
                (0, _) => p.to_ascii_lowercase(),
                (_, 2) => p.to_ascii_uppercase(),
                (_, 4) => {
                    let mut s = p[..1].to_ascii_uppercase();
                    s.push_str(&p[1..].to_ascii_lowercase());
                    s
                }
                _ => p.to_string(),
            })
            .collect::<Vec<_>>()
            .join("-"),
    )
}

/// The languages the recognizer offers, as (identifier, name, on-device):
/// valid tags only, each once, the system's language first and the rest by
/// name.
pub fn languages(
    reported: impl IntoIterator<Item = (String, String, bool)>,
    system: &str,
) -> Vec<Language> {
    let mut out: Vec<Language> = Vec::new();
    for (identifier, name, on_device) in reported {
        let Some(code) = bcp47(&identifier) else {
            continue;
        };
        if out.iter().any(|l| l.code == code) {
            continue;
        }
        let name = if name.trim().is_empty() {
            code.clone()
        } else {
            name.trim().to_string()
        };
        out.push(Language {
            code,
            name,
            on_device,
        });
    }
    let system = bcp47(system).unwrap_or_default();
    out.sort_by(|a, b| {
        (a.code != system)
            .cmp(&(b.code != system))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// How a recognition starts, as the page asks for it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartOptions {
    /// A BCP 47 tag, or empty for the system's language.
    #[serde(default)]
    pub language: String,
    /// Let the recognizer punctuate (macOS 13 and later).
    #[serde(default)]
    pub punctuation: bool,
    /// Use Apple's server when this Mac cannot recognize the language itself.
    #[serde(default)]
    pub allow_server: bool,
}

impl StartOptions {
    pub fn validate(self) -> Result<Self, String> {
        if !self.language.is_empty() && bcp47(&self.language).is_none() {
            return Err("unknown-language: That language is not one the Mac can recognize.".into());
        }
        Ok(self)
    }
}

/// What a recognition reports to the page that started it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    /// The text of one stretch of speech so far; `final` once it is settled.
    Result {
        session: u64,
        segment: u32,
        text: String,
        #[serde(rename = "final")]
        is_final: bool,
    },
    /// The microphone's level, 0 to 1, a few times a second.
    Level { session: u64, level: f32 },
    Error {
        session: u64,
        code: String,
        message: String,
    },
    /// The recognition is over and the microphone is closed.
    End { session: u64 },
}

pub const EVENT: &str = "voice-event";

/// The loudness of a buffer of samples on a 0–1 scale: its RMS in decibels,
/// from -55 dB (silence) to -5 dB (shouting into the microphone).
pub fn level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean = samples
        .iter()
        .filter(|s| s.is_finite())
        .map(|s| s * s)
        .sum::<f32>()
        / samples.len() as f32;
    let db = 10.0 * mean.max(1e-12).log10();
    ((db + 55.0) / 50.0).clamp(0.0, 1.0)
}

/// The words of a transcription as they are compared: lowercase letters and
/// digits, with where each one ends in the text (punctuation and case are
/// what the recognizer revises most, and never make a new utterance).
fn words(text: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut word = String::new();
    for (at, c) in text.char_indices() {
        if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        } else if !word.is_empty() {
            out.push((std::mem::take(&mut word), at));
        }
    }
    if !word.is_empty() {
        out.push((word, text.len()));
    }
    out
}

/// `shorter` is how `longer` begins: every word the same, the last one
/// possibly still being said ("log" for "logs").
fn begins(shorter: &[(String, usize)], longer: &[(String, usize)]) -> bool {
    shorter.len() <= longer.len()
        && shorter.iter().zip(longer).enumerate().all(|(i, (a, b))| {
            a.0 == b.0 || (i + 1 == shorter.len() && b.0.starts_with(a.0.as_str()))
        })
}

/// Whether `next` starts a new utterance rather than revising `previous`.
///
/// After a pause the recognizer starts over: on current macOS the best
/// transcription then holds only the new utterance. A revision of the same
/// utterance keeps its length within a word, or begins as it did; a new one
/// is shorter (it was just started) and is not how the previous one began.
/// `speechRecognitionMetadata` marks the end of an utterance where macOS sets
/// it; this is the check for when it does not.
pub fn starts_over(previous: &str, next: &str) -> bool {
    let (previous, next) = (words(previous), words(next));
    if previous.is_empty() || next.is_empty() {
        return false;
    }
    next.len() + 1 < previous.len() || (next.len() < previous.len() && !begins(&next, &previous))
}

/// One stretch of speech as the page receives it (`Event::Result`).
#[derive(Clone, Debug, PartialEq)]
pub struct Piece {
    pub segment: u32,
    pub text: String,
    pub is_final: bool,
}

/// Turns the recognizer's results into numbered stretches of speech, so a
/// pause never replaces what was said before it: a new utterance (or a new
/// recognition task after the old one ended) is the next segment, and the
/// one before is final first.
#[derive(Debug, Default)]
pub struct Segmenter {
    segment: u32,
    /// The latest text of the open segment ('' when none is open).
    open: String,
    /// What this recognition task already closed: a recognizer that keeps the
    /// whole task in one transcription repeats it before the new words.
    closed: String,
    heard_in_task: bool,
    heard: bool,
}

impl Segmenter {
    /// One result: `utterance_end` when the recognizer marks it as the last of an utterance.
    pub fn result(&mut self, text: &str, is_final: bool, utterance_end: bool) -> Vec<Piece> {
        let mut out = Vec::new();
        let text = self.without_closed(text.trim());
        let ends = is_final || utterance_end;
        if text.is_empty() {
            if ends {
                self.close(&mut out);
            }
            return out;
        }
        if starts_over(&self.open, &text) {
            self.close(&mut out);
        }
        self.heard = true;
        self.heard_in_task = true;
        out.push(Piece {
            segment: self.segment,
            text: text.clone(),
            is_final: ends,
        });
        self.open = text;
        if ends {
            self.advance();
        }
        out
    }

    /// The recognition task is over (another may follow): an open segment is final.
    pub fn task_over(&mut self) -> Vec<Piece> {
        let mut out = Vec::new();
        self.close(&mut out);
        self.closed.clear();
        self.heard_in_task = false;
        out
    }

    /// The task that is ending heard something.
    pub fn heard_in_task(&self) -> bool {
        self.heard_in_task
    }

    /// Anything was heard since the recognition started.
    pub fn heard(&self) -> bool {
        self.heard
    }

    fn close(&mut self, out: &mut Vec<Piece>) {
        if self.open.is_empty() {
            return;
        }
        out.push(Piece {
            segment: self.segment,
            text: self.open.clone(),
            is_final: true,
        });
        self.advance();
    }

    fn advance(&mut self) {
        if !self.closed.is_empty() {
            self.closed.push(' ');
        }
        self.closed.push_str(&std::mem::take(&mut self.open));
        self.segment += 1;
    }

    /// `text` without what this task already closed, when it repeats it first.
    fn without_closed(&self, text: &str) -> String {
        let closed = words(&self.closed);
        let said = words(text);
        if closed.is_empty() || said.len() < closed.len() {
            return text.to_string();
        }
        if !closed.iter().zip(&said).all(|(a, b)| a.0 == b.0) {
            return text.to_string();
        }
        let end = said[closed.len() - 1].1;
        text[end..]
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .to_string()
    }
}

/// How a recognition task ended.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskEnd {
    /// Its final result came (or it reported "ended": 203, 216, 301).
    Settled,
    /// 1110: it heard no speech.
    NoSpeech,
    /// 1700: macOS Dictation is off.
    DictationOff,
    Failed(String),
}

impl TaskEnd {
    pub fn from_error(code: isize, description: String) -> Self {
        match code {
            1110 => Self::NoSpeech,
            // kAFAssistantErrorDomain 1700: "Siri and Dictation are disabled".
            1700 => Self::DictationOff,
            // 203/216/301: the task was cancelled or ended early.
            203 | 216 | 301 => Self::Settled,
            _ => Self::Failed(description),
        }
    }
}

/// What happens after a recognition task ends.
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    /// The person has not stopped: a new task goes on listening, on the same microphone.
    Listen,
    /// The recognition is over.
    End,
    /// Over, with a problem to report first: (code, message).
    Report(&'static str, String),
}

/// How long listening goes on with nothing heard, across new tasks, before
/// it ends by itself: a forgotten microphone does not stay open.
pub const QUIET_LIMIT_MS: u64 = 60_000;
/// New tasks in a row that hear nothing: one that ends at once is not retried forever.
pub const QUIET_TASKS: u32 = 5;

/// The recognizer ends a task on its own after a pause (a final result, or
/// "no speech"); unless the person stopped, listening goes on in a new task.
/// "Nothing was heard" is said only when nothing was, since the start.
pub fn after_task(
    end: &TaskEnd,
    stopping: bool,
    heard: bool,
    quiet_ms: u64,
    quiet_tasks: u32,
) -> Next {
    match end {
        TaskEnd::DictationOff => return Next::Report("dictation-off", DICTATION_OFF.into()),
        TaskEnd::Failed(message) => return Next::Report("recognition", message.clone()),
        TaskEnd::NoSpeech if !heard => {
            return Next::Report("no-speech", "Nothing was heard.".into());
        }
        _ => {}
    }
    if stopping || quiet_ms >= QUIET_LIMIT_MS || quiet_tasks >= QUIET_TASKS {
        Next::End
    } else {
        Next::Listen
    }
}

/// System Settings' privacy pane for one permission. Only these two URLs can
/// be opened: the page names a pane, never a URL.
/// What to say when macOS's Dictation is off: its recognizer will not run without it.
const DICTATION_OFF: &str = "Voice uses macOS Dictation, which is turned off. Turn it on in System Settings → Keyboard → Dictation (macOS downloads its on-device model the first time), then try again.";

pub fn settings_url(pane: &str) -> Result<&'static str, String> {
    match pane {
        "microphone" => {
            Ok("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
        }
        "speech" => {
            Ok("x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition")
        }
        // Keyboard → Dictation: macOS's recognizer refuses to run while Dictation is off.
        "keyboard" => Ok("x-apple.systempreferences:com.apple.Keyboard-Settings.extension"),
        _ => Err("unknown settings pane".into()),
    }
}

/// What to read aloud and how.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SpeakOptions {
    pub text: String,
    /// A voice identifier from `speech_voices`, or empty for the default.
    #[serde(default)]
    pub voice: String,
    /// 0.5 (slow) to 2 (fast); 1 is the system's normal pace.
    #[serde(default = "normal_rate")]
    pub rate: f32,
}
fn normal_rate() -> f32 {
    1.0
}

pub const SPEAK_LIMIT: usize = 20_000;

impl SpeakOptions {
    pub fn validate(mut self) -> Result<Self, String> {
        if self.text.trim().is_empty() {
            return Err("Nothing to read aloud.".into());
        }
        if self.text.chars().count() > SPEAK_LIMIT {
            self.text = self.text.chars().take(SPEAK_LIMIT).collect();
        }
        if self.voice.len() > 256 || self.voice.chars().any(char::is_control) {
            return Err("Unknown voice.".into());
        }
        if !self.rate.is_finite() {
            self.rate = 1.0;
        }
        Ok(self)
    }
}

/// AVSpeechUtterance's rate for a pace multiplier: the system's default rate
/// times it, within the rates the synthesizer accepts.
pub fn utterance_rate(multiplier: f32, default: f32, min: f32, max: f32) -> f32 {
    let multiplier = if multiplier.is_finite() {
        multiplier.clamp(0.5, 2.0)
    } else {
        1.0
    };
    (default * multiplier).clamp(min, max)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Voice {
    pub id: String,
    pub name: String,
    pub language: String,
}

/// Runs `work` on the main thread, where every Speech and AVFoundation object
/// of this module lives, and waits for its answer.
async fn on_main<T: Send + 'static>(
    app: &AppHandle,
    wait: Duration,
    work: impl FnOnce(std::sync::mpsc::Sender<Result<T, String>>) + Send + 'static,
) -> Result<T, String> {
    let (send, receive) = std::sync::mpsc::channel::<Result<T, String>>();
    app.run_on_main_thread(move || work(send))
        .map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        receive
            .recv_timeout(wait)
            .map_err(|_| "The system did not answer in time.".to_string())?
    })
    .await
    .map_err(|e| e.to_string())?
}
const QUICK: Duration = Duration::from_secs(20);
/// A permission prompt waits for the person.
const PROMPT: Duration = Duration::from_secs(600);

#[tauri::command]
pub async fn voice_status(app: AppHandle, language: String) -> Result<Status, String> {
    let language = if language.is_empty() {
        String::new()
    } else {
        bcp47(&language).ok_or("unknown language")?
    };
    on_main(&app, QUICK, move |send| {
        let _ = send.send(Ok(native::status(&language)));
    })
    .await
}

#[tauri::command]
pub async fn voice_request_permission(app: AppHandle, language: String) -> Result<Status, String> {
    let language = if language.is_empty() {
        String::new()
    } else {
        bcp47(&language).ok_or("unknown language")?
    };
    on_main(&app, PROMPT, move |send| {
        native::request_permission(language, send)
    })
    .await
}

#[tauri::command]
pub async fn voice_languages(app: AppHandle) -> Result<Vec<Language>, String> {
    on_main(&app, QUICK, |send| {
        let _ = send.send(Ok(native::languages()));
    })
    .await
}

#[tauri::command]
pub fn voice_open_settings(app: AppHandle, pane: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = settings_url(&pane)?;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// Starts a recognition for the calling webview; its events go to that
/// webview only. A recognition already running (another window's) is
/// cancelled first: one microphone, one dictation.
#[tauri::command]
pub async fn voice_start(
    app: AppHandle,
    webview: Webview,
    options: StartOptions,
) -> Result<u64, String> {
    let options = options.validate()?;
    let label = webview.label().to_string();
    let handle = app.clone();
    on_main(&app, QUICK, move |send| {
        let _ = send.send(native::start(handle, label, options));
    })
    .await
}

/// Stops listening: the recognizer settles what it heard and ends.
#[tauri::command]
pub async fn voice_stop(app: AppHandle, session: u64) -> Result<(), String> {
    on_main(&app, QUICK, move |send| {
        native::stop(session);
        let _ = send.send(Ok(()));
    })
    .await
}

/// Stops listening and drops what was heard.
#[tauri::command]
pub async fn voice_cancel(app: AppHandle, session: u64) -> Result<(), String> {
    let handle = app.clone();
    on_main(&app, QUICK, move |send| {
        native::cancel(&handle, session);
        let _ = send.send(Ok(()));
    })
    .await
}

#[tauri::command]
pub async fn speech_voices(app: AppHandle) -> Result<Vec<Voice>, String> {
    on_main(&app, QUICK, |send| {
        let _ = send.send(Ok(native::voices()));
    })
    .await
}

#[tauri::command]
pub async fn speech_speak(app: AppHandle, options: SpeakOptions) -> Result<(), String> {
    let options = options.validate()?;
    on_main(&app, QUICK, move |send| {
        let _ = send.send(native::speak(&options));
    })
    .await
}

#[tauri::command]
pub async fn speech_stop(app: AppHandle) -> Result<(), String> {
    on_main(&app, QUICK, |send| {
        native::stop_speaking();
        let _ = send.send(Ok(()));
    })
    .await
}

#[tauri::command]
pub async fn speech_speaking(app: AppHandle) -> Result<bool, String> {
    on_main(&app, QUICK, |send| {
        let _ = send.send(Ok(native::speaking()));
    })
    .await
}

#[cfg(target_os = "macos")]
mod native {
    use super::{
        Event, Language, Next, Permission, Piece, Segmenter, SpeakOptions, StartOptions, Status,
        TaskEnd, Voice,
    };
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, NSObjectProtocol};
    use objc2::{AnyThread, sel};
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
    use objc2_avf_audio::{
        AVAudioEngine, AVAudioPCMBuffer, AVAudioTime, AVSpeechBoundary, AVSpeechSynthesisVoice,
        AVSpeechSynthesizer, AVSpeechUtterance, AVSpeechUtteranceDefaultSpeechRate,
        AVSpeechUtteranceMaximumSpeechRate, AVSpeechUtteranceMinimumSpeechRate,
    };
    use objc2_foundation::{NSError, NSLocale, NSString};
    use objc2_speech::{
        SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask,
        SFSpeechRecognizer,
    };
    use std::cell::{Cell, RefCell};
    use std::ptr::NonNull;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tauri::{AppHandle, Emitter, EventTarget};

    type Request = Retained<SFSpeechAudioBufferRecognitionRequest>;

    /// The request the microphone tap appends to, shared with the audio thread.
    struct Feed(Request);
    // SAFETY: the audio tap appends buffers from its own thread, as the Speech
    // framework intends `appendAudioPCMBuffer` to be used (the request was
    // already shared with the tap that way); the main thread only swaps the
    // request under the mutex and ends the old one's audio.
    unsafe impl Send for Feed {}

    /// One recognition, from the page's start to its stop or cancel. The
    /// recognizer may end its task on a pause; the recognition then goes on
    /// in a new task, fed by the same microphone tap (`feed`). It lives on the
    /// main thread, where the result handlers run (the recognizer's default
    /// queue is the main queue; `ACTIVE` already relies on it).
    struct Recognition {
        id: u64,
        app: AppHandle,
        /// The webview that started it, which alone hears its events.
        label: String,
        recognizer: Retained<SFSpeechRecognizer>,
        on_device: bool,
        punctuation: bool,
        /// The request the microphone tap appends to: the current task's.
        feed: Arc<Mutex<Feed>>,
        segmenter: RefCell<Segmenter>,
        /// The current task; a superseded task's late callbacks are ignored.
        generation: Cell<u32>,
        /// Set when the page cancelled: the error the cancelled task reports is not news.
        cancelled: Cell<bool>,
        /// Set when the page stopped: the task's final result ends the recognition.
        stopping: Cell<bool>,
        /// The page heard its end.
        finished: Cell<bool>,
        /// When something was last heard (or the recognition started).
        last_heard: Cell<u64>,
        /// New tasks in a row that heard nothing.
        quiet_tasks: Cell<u32>,
    }

    struct Active {
        id: u64,
        engine: Retained<AVAudioEngine>,
        recognition: Rc<Recognition>,
        task: Retained<SFSpeechRecognitionTask>,
        listening: bool,
    }

    thread_local! {
        static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
        static SYNTH: RefCell<Option<Retained<AVSpeechSynthesizer>>> = const { RefCell::new(None) };
        static LANGUAGES: RefCell<Option<Vec<Language>>> = const { RefCell::new(None) };
    }
    static SESSIONS: AtomicU64 = AtomicU64::new(0);

    fn emit(app: &AppHandle, label: &str, event: Event) {
        let _ = app.emit_to(EventTarget::webview(label), super::EVENT, event);
    }

    fn system_language() -> String {
        NSLocale::preferredLanguages()
            .iter()
            .next()
            .and_then(|l| super::bcp47(&l.to_string()))
            .unwrap_or_default()
    }

    fn recognizer(language: &str) -> Option<Retained<SFSpeechRecognizer>> {
        unsafe {
            if language.is_empty() {
                SFSpeechRecognizer::init(SFSpeechRecognizer::alloc())
            } else {
                let locale = NSLocale::localeWithLocaleIdentifier(&NSString::from_str(language));
                SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &locale)
            }
        }
    }

    fn microphone() -> Permission {
        match unsafe { AVMediaTypeAudio } {
            Some(audio) => Permission::from_capture(
                unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) }.0,
            ),
            None => Permission::Unsupported,
        }
    }

    pub fn status(language: &str) -> Status {
        let speech =
            Permission::from_speech(unsafe { SFSpeechRecognizer::authorizationStatus() }.0);
        let microphone = microphone();
        let system = system_language();
        let Some(recognizer) = recognizer(language) else {
            return Status {
                available: false,
                on_device: false,
                microphone,
                speech,
                reason: "This Mac does not recognize that language.".into(),
                language: system,
            };
        };
        let available = unsafe { recognizer.isAvailable() };
        Status {
            available,
            on_device: unsafe { recognizer.supportsOnDeviceRecognition() },
            microphone,
            speech,
            reason: if available {
                String::new()
            } else {
                "Speech recognition is not available right now.".into()
            },
            language: system,
        }
    }

    /// Asks for speech recognition, then for the microphone, each only if
    /// the person was never asked; answers with the status after both.
    pub fn request_permission(
        language: String,
        send: std::sync::mpsc::Sender<Result<Status, String>>,
    ) {
        let after_speech = RcBlock::new(
            move |_status: objc2_speech::SFSpeechRecognizerAuthorizationStatus| {
                let send = send.clone();
                let language = language.clone();
                let done = RcBlock::new(move |_granted: Bool| {
                    let send = send.clone();
                    let language = language.clone();
                    // The status reads the recognizer, which lives on the main thread.
                    dispatch_main(move || {
                        let _ = send.send(Ok(status(&language)));
                    });
                });
                match unsafe { AVMediaTypeAudio } {
                    Some(audio) => unsafe {
                        AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &done)
                    },
                    None => done.call((Bool::NO,)),
                }
            },
        );
        unsafe { SFSpeechRecognizer::requestAuthorization(&after_speech) };
    }

    /// A closure to the main queue (the permission handlers answer on any thread).
    fn dispatch_main(work: impl FnOnce() + Send + 'static) {
        use objc2_foundation::NSOperationQueue;
        let work = std::cell::Cell::new(Some(work));
        let block = RcBlock::new(move || {
            if let Some(work) = work.take() {
                work();
            }
        });
        unsafe { NSOperationQueue::mainQueue().addOperationWithBlock(&block) };
    }

    /// Asking each language whether it is on-device takes a quarter of a
    /// second on the main thread: once per launch is enough.
    pub fn languages() -> Vec<Language> {
        if let Some(known) = LANGUAGES.with(|l| l.borrow().clone()) {
            return known;
        }
        let list = read_languages();
        LANGUAGES.with(|l| *l.borrow_mut() = Some(list.clone()));
        list
    }

    fn read_languages() -> Vec<Language> {
        let current = NSLocale::currentLocale();
        let reported: Vec<(String, String, bool)> =
            unsafe { SFSpeechRecognizer::supportedLocales() }
                .iter()
                .map(|locale| {
                    let identifier = locale.localeIdentifier();
                    let name = current
                        .localizedStringForLocaleIdentifier(&identifier)
                        .to_string();
                    let on_device = unsafe {
                        SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &locale)
                            .map(|r| r.supportsOnDeviceRecognition())
                            .unwrap_or(false)
                    };
                    (identifier.to_string(), name, on_device)
                })
                .collect();
        super::languages(reported, &system_language())
    }

    fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn release(active: &mut Active) {
        if active.listening {
            active.listening = false;
            let request = current_request(&active.recognition);
            unsafe {
                active.engine.inputNode().removeTapOnBus(0);
                active.engine.stop();
                request.endAudio();
            }
        }
    }

    fn current_request(recognition: &Recognition) -> Request {
        lock(&recognition.feed).0.clone()
    }

    fn new_request(on_device: bool, punctuation: bool) -> Request {
        let request = unsafe { SFSpeechAudioBufferRecognitionRequest::new() };
        unsafe {
            request.setShouldReportPartialResults(true);
            request.setRequiresOnDeviceRecognition(on_device);
            if punctuation && request.respondsToSelector(sel!(setAddsPunctuation:)) {
                request.setAddsPunctuation(true);
            }
        }
        request
    }

    pub fn start(app: AppHandle, label: String, options: StartOptions) -> Result<u64, String> {
        let speech =
            Permission::from_speech(unsafe { SFSpeechRecognizer::authorizationStatus() }.0);
        if speech != Permission::Granted {
            return Err(
                "speech-permission: Allow speech recognition for Lazybox in Settings → Voice."
                    .into(),
            );
        }
        if microphone() != Permission::Granted {
            return Err(
                "microphone-permission: Allow the microphone for Lazybox in Settings → Voice."
                    .into(),
            );
        }
        let language = super::bcp47(&options.language).unwrap_or_default();
        let recognizer = recognizer(&language)
            .ok_or("unknown-language: This Mac does not recognize that language.")?;
        if !unsafe { recognizer.isAvailable() } {
            return Err("unavailable: Speech recognition is not available right now. Try again in a moment.".into());
        }
        let on_device = unsafe { recognizer.supportsOnDeviceRecognition() };
        if !on_device && !options.allow_server {
            return Err("on-device-unavailable: This Mac cannot recognize that language on-device. Add it in System Settings → Keyboard → Dictation, or allow Apple's server in Settings → Voice.".into());
        }
        // One microphone, one dictation: a recognition another window left running ends here.
        if let Some(previous) = ACTIVE.with(|a| a.borrow().as_ref().map(|a| a.id)) {
            cancel(&app, previous);
        }
        let id = SESSIONS.fetch_add(1, Ordering::Relaxed) + 1;
        let request = new_request(on_device, options.punctuation);
        let engine = unsafe { AVAudioEngine::new() };
        let input = unsafe { engine.inputNode() };
        let format = unsafe { input.outputFormatForBus(0) };
        if unsafe { format.sampleRate() } <= 0.0 || unsafe { format.channelCount() } == 0 {
            return Err("no-microphone: No microphone is connected.".into());
        }
        let feed = Arc::new(Mutex::new(Feed(request.clone())));
        // The tap runs on an audio thread: it feeds the current task's request
        // and, a few times a second, reports the level.
        let tap_feed = feed.clone();
        let (tap_app, tap_label) = (app.clone(), label.clone());
        let last = Arc::new(AtomicU64::new(0));
        let tap = RcBlock::new(
            move |buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
                let buffer = unsafe { buffer.as_ref() };
                if let Ok(feed) = tap_feed.lock() {
                    unsafe { feed.0.appendAudioPCMBuffer(buffer) };
                }
                let now = now_millis();
                if now.saturating_sub(last.load(Ordering::Relaxed)) < 70 {
                    return;
                }
                last.store(now, Ordering::Relaxed);
                let level = unsafe {
                    let channels = buffer.floatChannelData();
                    let frames = buffer.frameLength() as usize;
                    if channels.is_null() || frames == 0 {
                        0.0
                    } else {
                        super::level(std::slice::from_raw_parts((*channels).as_ptr(), frames))
                    }
                };
                emit(&tap_app, &tap_label, Event::Level { session: id, level });
            },
        );
        unsafe {
            input.installTapOnBus_bufferSize_format_block(
                0,
                1024,
                Some(&format),
                &*tap as *const _ as *mut _,
            );
            engine.prepare();
        }
        if let Err(error) = unsafe { engine.startAndReturnError() } {
            unsafe { input.removeTapOnBus(0) };
            return Err(format!(
                "microphone: The microphone could not start: {}",
                error.localizedDescription()
            ));
        }
        let recognition = Rc::new(Recognition {
            id,
            app,
            label,
            recognizer,
            on_device,
            punctuation: options.punctuation,
            feed,
            segmenter: RefCell::new(Segmenter::default()),
            generation: Cell::new(0),
            cancelled: Cell::new(false),
            stopping: Cell::new(false),
            finished: Cell::new(false),
            last_heard: Cell::new(now_millis()),
            quiet_tasks: Cell::new(0),
        });
        let task = begin_task(&recognition, &request, 0);
        ACTIVE.with(|a| {
            *a.borrow_mut() = Some(Active {
                id,
                engine,
                recognition,
                task,
                listening: true,
            })
        });
        Ok(id)
    }

    /// A recognition task on `request`; its results and its end come to `handle`.
    fn begin_task(
        recognition: &Rc<Recognition>,
        request: &Request,
        generation: u32,
    ) -> Retained<SFSpeechRecognitionTask> {
        let own = recognition.clone();
        let handler = RcBlock::new(
            move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
                handle(&own, generation, unsafe { result.as_ref() }, unsafe {
                    error.as_ref()
                });
            },
        );
        unsafe {
            recognition
                .recognizer
                .recognitionTaskWithRequest_resultHandler(request, &handler)
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn send(recognition: &Recognition, pieces: Vec<Piece>) {
        for piece in pieces {
            emit(
                &recognition.app,
                &recognition.label,
                Event::Result {
                    session: recognition.id,
                    segment: piece.segment,
                    text: piece.text,
                    is_final: piece.is_final,
                },
            );
        }
    }

    /// One callback of a task: results become numbered stretches of speech
    /// (`Segmenter`), and the task's end either ends the recognition or
    /// starts the next task (`after_task`).
    fn handle(
        recognition: &Rc<Recognition>,
        generation: u32,
        result: Option<&SFSpeechRecognitionResult>,
        error: Option<&NSError>,
    ) {
        if recognition.cancelled.get()
            || recognition.finished.get()
            || recognition.generation.get() != generation
        {
            return;
        }
        let mut over = false;
        if let Some(result) = result {
            let text = unsafe { result.bestTranscription().formattedString() }.to_string();
            let is_final = unsafe { result.isFinal() };
            // Set on the last result of an utterance (macOS 10.15 and later).
            let utterance_end = result.respondsToSelector(sel!(speechRecognitionMetadata))
                && unsafe { result.speechRecognitionMetadata() }.is_some();
            if !text.trim().is_empty() {
                recognition.last_heard.set(now_millis());
            }
            let pieces = recognition
                .segmenter
                .borrow_mut()
                .result(&text, is_final, utterance_end);
            send(recognition, pieces);
            over = is_final;
        }
        let mut end = TaskEnd::Settled;
        if let Some(error) = error {
            over = true;
            end = TaskEnd::from_error(error.code(), error.localizedDescription().to_string());
        }
        if !over {
            return;
        }
        let (pieces, heard_in_task, heard) = {
            let mut segmenter = recognition.segmenter.borrow_mut();
            let heard_in_task = segmenter.heard_in_task();
            (segmenter.task_over(), heard_in_task, segmenter.heard())
        };
        send(recognition, pieces);
        let quiet_tasks = if heard_in_task {
            recognition.quiet_tasks.set(0);
            0
        } else {
            recognition
                .quiet_tasks
                .set(recognition.quiet_tasks.get() + 1);
            recognition.quiet_tasks.get()
        };
        let quiet_ms = now_millis().saturating_sub(recognition.last_heard.get());
        let stopping = recognition.stopping.get();
        match super::after_task(&end, stopping, heard, quiet_ms, quiet_tasks) {
            Next::Listen => listen_again(recognition),
            Next::End => finish(recognition),
            Next::Report(code, message) => {
                emit(
                    &recognition.app,
                    &recognition.label,
                    Event::Error {
                        session: recognition.id,
                        code: code.into(),
                        message,
                    },
                );
                finish(recognition);
            }
        }
    }

    /// The recognizer ended its task on a pause and the person has not
    /// stopped: a new task goes on with the same microphone and tap, so the
    /// pill never blinks and the words after the pause are the next segment.
    fn listen_again(recognition: &Rc<Recognition>) {
        let request = new_request(recognition.on_device, recognition.punctuation);
        let generation = recognition.generation.get() + 1;
        recognition.generation.set(generation);
        // The tap feeds the new request from its next buffer on.
        let old = std::mem::replace(&mut *lock(&recognition.feed), Feed(request.clone()));
        unsafe { old.0.endAudio() };
        let task = begin_task(recognition, &request, generation);
        let kept = ACTIVE.with(|a| match a.try_borrow_mut() {
            Ok(mut slot) => match slot.as_mut().filter(|a| a.id == recognition.id) {
                Some(active) => {
                    active.task = task.clone();
                    true
                }
                None => false,
            },
            Err(_) => false,
        });
        // Stopped or cancelled meanwhile: the new task has nothing to do.
        if !kept {
            recognition.generation.set(generation + 1);
            unsafe { task.cancel() };
            finish(recognition);
        }
    }

    /// The recognition ended (settled, failed or cancelled): the microphone
    /// closes and the page hears that it is over, once.
    fn finish(recognition: &Recognition) {
        if recognition.finished.replace(true) {
            return;
        }
        // Taken out before any call into the frameworks: one may call the
        // result handler, which comes back here, while the slot is borrowed.
        if let Some(mut active) = take(recognition.id) {
            release(&mut active);
        }
        emit(
            &recognition.app,
            &recognition.label,
            Event::End {
                session: recognition.id,
            },
        );
    }

    fn take(id: u64) -> Option<Active> {
        ACTIVE.with(|a| {
            let mut slot = a.borrow_mut();
            if slot.as_ref().is_some_and(|a| a.id == id) {
                slot.take()
            } else {
                None
            }
        })
    }

    /// Closes the microphone; the recognizer settles what it heard, and its
    /// final result ends the recognition (`finish`).
    pub fn stop(id: u64) {
        if let Some(mut active) = take(id) {
            active.recognition.stopping.set(true);
            release(&mut active);
            // The final result may have come while it was out of the slot.
            if active.recognition.finished.get() {
                return;
            }
            ACTIVE.with(|a| {
                let mut slot = a.borrow_mut();
                if slot.is_none() {
                    *slot = Some(active);
                }
            });
        }
    }

    pub fn cancel(app: &AppHandle, id: u64) {
        if let Some(mut active) = take(id) {
            let recognition = active.recognition.clone();
            recognition.cancelled.set(true);
            let already = recognition.finished.replace(true);
            release(&mut active);
            unsafe { active.task.cancel() };
            if !already {
                emit(app, &recognition.label, Event::End { session: id });
            }
        }
    }

    fn synthesizer() -> Retained<AVSpeechSynthesizer> {
        SYNTH.with(|s| {
            s.borrow_mut()
                .get_or_insert_with(|| unsafe { AVSpeechSynthesizer::new() })
                .clone()
        })
    }

    pub fn voices() -> Vec<Voice> {
        let mut voices: Vec<Voice> = unsafe { AVSpeechSynthesisVoice::speechVoices() }
            .iter()
            .map(|v| unsafe {
                Voice {
                    id: v.identifier().to_string(),
                    name: v.name().to_string(),
                    language: super::bcp47(&v.language().to_string()).unwrap_or_default(),
                }
            })
            .filter(|v| !v.language.is_empty())
            .collect();
        voices.sort_by(|a, b| {
            a.language
                .cmp(&b.language)
                .then_with(|| a.name.cmp(&b.name))
        });
        voices
    }

    pub fn speak(options: &SpeakOptions) -> Result<(), String> {
        let synth = synthesizer();
        unsafe {
            synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate);
            let utterance =
                AVSpeechUtterance::speechUtteranceWithString(&NSString::from_str(&options.text));
            if !options.voice.is_empty() {
                let voice = AVSpeechSynthesisVoice::voiceWithIdentifier(&NSString::from_str(
                    &options.voice,
                ))
                .ok_or("That voice is not installed on this Mac.")?;
                utterance.setVoice(Some(&voice));
            }
            utterance.setRate(super::utterance_rate(
                options.rate,
                AVSpeechUtteranceDefaultSpeechRate,
                AVSpeechUtteranceMinimumSpeechRate,
                AVSpeechUtteranceMaximumSpeechRate,
            ));
            synth.speakUtterance(&utterance);
        }
        Ok(())
    }

    pub fn stop_speaking() {
        SYNTH.with(|s| {
            if let Some(synth) = s.borrow().as_ref() {
                unsafe { synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate) };
            }
        });
    }

    pub fn speaking() -> bool {
        SYNTH.with(|s| {
            s.borrow()
                .as_ref()
                .is_some_and(|synth| unsafe { synth.isSpeaking() })
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::{Language, SpeakOptions, StartOptions, Status, Voice};
    use tauri::AppHandle;
    const UNSUPPORTED: &str =
        "Voice in the desktop app needs macOS. Use the browser client for Web Speech.";
    pub fn status(_language: &str) -> Status {
        Status::unsupported(UNSUPPORTED)
    }
    pub fn request_permission(
        _language: String,
        send: std::sync::mpsc::Sender<Result<Status, String>>,
    ) {
        let _ = send.send(Ok(Status::unsupported(UNSUPPORTED)));
    }
    pub fn languages() -> Vec<Language> {
        Vec::new()
    }
    pub fn start(_app: AppHandle, _label: String, _options: StartOptions) -> Result<u64, String> {
        Err(format!("unsupported: {UNSUPPORTED}"))
    }
    pub fn stop(_id: u64) {}
    pub fn cancel(_app: &AppHandle, _id: u64) {}
    pub fn voices() -> Vec<Voice> {
        Vec::new()
    }
    pub fn speak(_options: &SpeakOptions) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    pub fn stop_speaking() {}
    pub fn speaking() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_states_follow_each_framework_numbering() {
        // SFSpeechRecognizerAuthorizationStatus
        assert_eq!(Permission::from_speech(0), Permission::NotDetermined);
        assert_eq!(Permission::from_speech(1), Permission::Denied);
        assert_eq!(Permission::from_speech(2), Permission::Restricted);
        assert_eq!(Permission::from_speech(3), Permission::Granted);
        // AVAuthorizationStatus swaps denied and restricted.
        assert_eq!(Permission::from_capture(0), Permission::NotDetermined);
        assert_eq!(Permission::from_capture(1), Permission::Restricted);
        assert_eq!(Permission::from_capture(2), Permission::Denied);
        assert_eq!(Permission::from_capture(3), Permission::Granted);
        for raw in [-1, 4, 99] {
            assert_eq!(Permission::from_speech(raw), Permission::Unsupported);
            assert_eq!(Permission::from_capture(raw), Permission::Unsupported);
        }
        assert_eq!(
            serde_json::to_value(Permission::NotDetermined).unwrap(),
            "not-determined"
        );
        let status = serde_json::to_value(Status::unsupported("No")).unwrap();
        assert_eq!(status["onDevice"], false);
        assert_eq!(status["microphone"], "unsupported");
    }

    #[test]
    fn locale_identifiers_become_bcp47_tags_and_nothing_else_passes() {
        assert_eq!(bcp47("en_US").as_deref(), Some("en-US"));
        assert_eq!(bcp47("fr-fr").as_deref(), Some("fr-FR"));
        assert_eq!(bcp47("zh_hans_CN").as_deref(), Some("zh-Hans-CN"));
        assert_eq!(bcp47("yue_CN").as_deref(), Some("yue-CN"));
        assert_eq!(bcp47("es-419").as_deref(), Some("es-419"));
        assert_eq!(bcp47("fr").as_deref(), Some("fr"));
        for invalid in [
            "",
            "e",
            "english",
            "en US",
            "en-",
            "en--US",
            "../en",
            "en-US;rm",
            "12-US",
            "en-a-b-c-d",
        ] {
            assert_eq!(bcp47(invalid), None, "{invalid}");
        }
        assert!(
            StartOptions {
                language: "fr_FR".into(),
                punctuation: true,
                allow_server: false
            }
            .validate()
            .is_ok()
        );
        assert!(
            StartOptions {
                language: String::new(),
                punctuation: false,
                allow_server: false
            }
            .validate()
            .is_ok()
        );
        let refused = StartOptions {
            language: "fr FR".into(),
            punctuation: false,
            allow_server: false,
        }
        .validate();
        assert!(refused.unwrap_err().starts_with("unknown-language:"));
        let options: Result<StartOptions, _> =
            serde_json::from_str(r#"{"language":"en-US","record":true}"#);
        assert!(options.is_err(), "unknown fields are refused");
    }

    #[test]
    fn language_list_is_valid_unique_system_first_then_by_name() {
        let reported = [
            ("fr_FR", "French (France)", true),
            ("en_US", "English (United States)", true),
            ("de_DE", "German (Germany)", false),
            ("en-US", "English (US) again", true),
            ("bad tag", "Nonsense", true),
            ("ja_JP", "", false),
            ("en_GB", "English (United Kingdom)", true),
        ]
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c));
        let list = languages(reported, "fr-FR");
        let codes: Vec<&str> = list.iter().map(|l| l.code.as_str()).collect();
        assert_eq!(codes, ["fr-FR", "en-GB", "en-US", "de-DE", "ja-JP"]);
        assert_eq!(list[0].code, "fr-FR");
        // The rest by name; a missing name is its code.
        let names: Vec<&str> = list[1..].iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "English (United Kingdom)",
                "English (United States)",
                "German (Germany)",
                "ja-JP"
            ]
        );
        assert!(
            !list
                .iter()
                .any(|l| l.name == "Nonsense" || l.name == "English (US) again")
        );
        assert!(!list.iter().find(|l| l.code == "de-DE").unwrap().on_device);
        let json = serde_json::to_value(&list[0]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "code": "fr-FR", "name": "French (France)", "onDevice": true })
        );
        // An unrecognizable system language leaves the order by name.
        assert_eq!(languages(Vec::new(), "??"), Vec::new());
    }

    #[test]
    fn events_serialize_as_the_page_reads_them() {
        let result = Event::Result {
            session: 3,
            segment: 0,
            text: "Bonjour".into(),
            is_final: true,
        };
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({ "kind": "result", "session": 3, "segment": 0, "text": "Bonjour", "final": true })
        );
        assert_eq!(
            serde_json::to_value(Event::End { session: 3 }).unwrap(),
            serde_json::json!({ "kind": "end", "session": 3 })
        );
        let error = Event::Error {
            session: 1,
            code: "no-speech".into(),
            message: "Nothing was heard.".into(),
        };
        assert_eq!(serde_json::to_value(error).unwrap()["code"], "no-speech");
    }

    fn piece(segment: u32, text: &str, is_final: bool) -> Piece {
        Piece {
            segment,
            text: text.into(),
            is_final,
        }
    }

    #[test]
    fn a_new_utterance_is_told_from_a_revision() {
        // Growing, or a word revised: the same utterance.
        assert!(!starts_over("look at the", "look at the logs"));
        assert!(!starts_over("look at the logs", "Look at the logs."));
        assert!(!starts_over("look at the logs", "look at the locks"));
        assert!(!starts_over("look at the logs and", "look at the logs"));
        assert!(!starts_over("fix the log", "fix the"));
        assert!(!starts_over("", "anything"));
        assert!(!starts_over("anything", ""));
        // After a pause the recognizer starts over with only the new words.
        assert!(starts_over("look at the logs", "and"));
        assert!(starts_over("look at the logs", "and fix"));
        assert!(starts_over("look at the logs please", "look"));
        assert!(starts_over("fix it", "and"));
    }

    #[test]
    fn a_pause_starts_the_next_segment_and_settles_the_one_before() {
        let mut s = Segmenter::default();
        assert_eq!(
            s.result("look at the", false, false),
            [piece(0, "look at the", false)]
        );
        assert_eq!(
            s.result("look at the logs", false, false),
            [piece(0, "look at the logs", false)]
        );
        // (pause) The recognizer starts over: segment 0 is final, then segment 1.
        assert_eq!(
            s.result("and fix", false, false),
            [
                piece(0, "look at the logs", true),
                piece(1, "and fix", false)
            ]
        );
        assert_eq!(
            s.result("and fix them", false, false),
            [piece(1, "and fix them", false)]
        );
        assert!(s.heard() && s.heard_in_task());
    }

    #[test]
    fn the_end_of_an_utterance_closes_its_segment() {
        let mut s = Segmenter::default();
        s.result("look at the", false, false);
        // speechRecognitionMetadata on the last result of the utterance.
        assert_eq!(
            s.result("look at the logs", false, true),
            [piece(0, "look at the logs", true)]
        );
        assert_eq!(s.result("and", false, false), [piece(1, "and", false)]);
        assert_eq!(
            s.result("and fix", false, false),
            [piece(1, "and fix", false)]
        );
        // A recognizer that keeps the whole task in one transcription repeats
        // what was closed: only the new words are the new segment.
        let mut s = Segmenter::default();
        s.result("look at the logs", false, true);
        assert_eq!(s.result("Look at the logs", false, false), []);
        assert_eq!(
            s.result("Look at the logs. And fix", false, false),
            [piece(1, "And fix", false)]
        );
        // An empty result never replaces words.
        assert_eq!(s.result("", false, false), []);
        assert_eq!(s.result("", true, false), [piece(1, "And fix", true)]);
    }

    #[test]
    fn a_new_task_goes_on_in_the_next_segment() {
        let mut s = Segmenter::default();
        s.result("look at the logs", false, false);
        // The task ended on the pause with no final: what it heard is final.
        assert_eq!(s.task_over(), [piece(0, "look at the logs", true)]);
        assert!(!s.heard_in_task() && s.heard());
        // The next task's words, even the same ones, are segment 1.
        assert_eq!(
            s.result("look at the logs", false, false),
            [piece(1, "look at the logs", false)]
        );
        assert_eq!(
            s.result("look at the logs again", true, false),
            [piece(1, "look at the logs again", true)]
        );
        assert_eq!(s.task_over(), []);
        assert_eq!(
            s.result("then send", false, false),
            [piece(2, "then send", false)]
        );
    }

    #[test]
    fn listening_goes_on_after_a_pause_until_the_person_stops() {
        // The task settled on a pause: a new one listens.
        assert_eq!(
            after_task(&TaskEnd::Settled, false, true, 2_000, 0),
            Next::Listen
        );
        // "No speech" after speech is a pause, not a problem.
        assert_eq!(
            after_task(&TaskEnd::NoSpeech, false, true, 5_000, 1),
            Next::Listen
        );
        // Nothing heard since the start: say so, as before.
        assert_eq!(
            after_task(&TaskEnd::NoSpeech, false, false, 5_000, 1),
            Next::Report("no-speech", "Nothing was heard.".into())
        );
        assert_eq!(
            after_task(&TaskEnd::NoSpeech, true, false, 500, 0),
            Next::Report("no-speech", "Nothing was heard.".into())
        );
        // The person stopped: the final result ends it.
        assert_eq!(after_task(&TaskEnd::Settled, true, true, 0, 0), Next::End);
        assert_eq!(after_task(&TaskEnd::NoSpeech, true, true, 0, 0), Next::End);
        // A forgotten microphone closes; a task that ends at once is not retried forever.
        assert_eq!(
            after_task(&TaskEnd::Settled, false, true, QUIET_LIMIT_MS, 1),
            Next::End
        );
        assert_eq!(
            after_task(&TaskEnd::Settled, false, true, 1_000, QUIET_TASKS),
            Next::End
        );
        // Problems are reported as before.
        assert_eq!(
            after_task(&TaskEnd::DictationOff, false, true, 0, 0),
            Next::Report("dictation-off", DICTATION_OFF.into())
        );
        assert_eq!(
            after_task(&TaskEnd::Failed("Busy".into()), false, true, 0, 0),
            Next::Report("recognition", "Busy".into())
        );
        assert_eq!(TaskEnd::from_error(1110, String::new()), TaskEnd::NoSpeech);
        assert_eq!(
            TaskEnd::from_error(1700, String::new()),
            TaskEnd::DictationOff
        );
        for ended in [203, 216, 301] {
            assert_eq!(TaskEnd::from_error(ended, String::new()), TaskEnd::Settled);
        }
        assert_eq!(
            TaskEnd::from_error(1, "Busy".into()),
            TaskEnd::Failed("Busy".into())
        );
    }

    #[test]
    fn level_is_a_decibel_scale_from_silence_to_loud() {
        assert_eq!(level(&[]), 0.0);
        assert_eq!(level(&[0.0; 512]), 0.0);
        let quiet = level(&[0.003; 512]);
        let speech = level(&[0.05; 512]);
        let loud = level(&[0.9; 512]);
        assert!(quiet < 0.2, "{quiet}");
        assert!(speech > quiet && speech < loud, "{speech}");
        assert_eq!(level(&[1.0; 16]), 1.0);
        assert_eq!(level(&[f32::NAN, 0.0]), 0.0);
    }

    #[test]
    fn only_the_two_privacy_panes_open() {
        assert!(
            settings_url("microphone")
                .unwrap()
                .ends_with("Privacy_Microphone")
        );
        assert!(
            settings_url("speech")
                .unwrap()
                .ends_with("Privacy_SpeechRecognition")
        );
        assert!(settings_url("https://example.com").is_err());
        assert!(settings_url("").is_err());
    }

    /// Reads the real Speech and AVFoundation frameworks (no prompt, no
    /// microphone): run by hand on a Mac with `cargo test -- --ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "reads this Mac's speech frameworks"]
    fn the_real_frameworks_answer_on_this_mac() {
        let list = native::languages();
        assert!(
            !list.is_empty(),
            "macOS offers speech recognition languages"
        );
        assert!(
            list.iter()
                .all(|l| bcp47(&l.code).as_deref() == Some(l.code.as_str()))
        );
        let status = native::status("");
        assert_ne!(status.speech, Permission::Unsupported);
        assert_ne!(status.microphone, Permission::Unsupported);
        assert!(!native::voices().is_empty(), "macOS has voices");
        eprintln!(
            "{} languages; system {}; on-device {}; {:?}",
            list.len(),
            status.language,
            status.on_device,
            status
        );
    }

    #[test]
    fn read_aloud_options_are_bounded() {
        let options: SpeakOptions = serde_json::from_str(r#"{"text":"Hello"}"#).unwrap();
        assert_eq!(options.rate, 1.0);
        assert!(
            SpeakOptions {
                text: "  ".into(),
                voice: String::new(),
                rate: 1.0
            }
            .validate()
            .is_err()
        );
        let long = SpeakOptions {
            text: "a".repeat(SPEAK_LIMIT + 10),
            voice: String::new(),
            rate: f32::NAN,
        }
        .validate()
        .unwrap();
        assert_eq!(long.text.len(), SPEAK_LIMIT);
        assert_eq!(long.rate, 1.0);
        assert!(
            SpeakOptions {
                text: "Hi".into(),
                voice: "bad\nvoice".into(),
                rate: 1.0
            }
            .validate()
            .is_err()
        );
        // AVSpeechUtterance: default 0.5, range 0–1.
        assert_eq!(utterance_rate(1.0, 0.5, 0.0, 1.0), 0.5);
        assert_eq!(utterance_rate(2.0, 0.5, 0.0, 1.0), 1.0);
        assert_eq!(utterance_rate(9.0, 0.5, 0.0, 1.0), 1.0);
        assert_eq!(utterance_rate(0.1, 0.5, 0.0, 1.0), 0.25);
        assert_eq!(utterance_rate(f32::INFINITY, 0.5, 0.0, 1.0), 0.5);
    }
}
