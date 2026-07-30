// @ts-check
// Shared logic for the demo-video caption overlay. Kept DOM-free so the same
// cue-parsing and active-caption rules that drive index.astro's overlay are
// unit-tested directly (see scripts/demo-captions.test.mjs). The WebVTT <track>
// is the single source of truth; this module only decides *what* to show.

/**
 * Split a cue into an optional keycap chip and its description. A cue authored
 * as "⌨ <chord> — <text>" yields a chip; anything else is description-only, so
 * context cues that merely contain an em dash are never mistaken for a chord.
 *
 * @param {string} text
 * @returns {{ key: string | null, text: string }}
 */
export function captionParts(text) {
  const m = /^⌨\s+(.+?)\s+—\s+([\s\S]+)$/.exec(text);
  return m ? { key: m[1], text: m[2] } : { key: null, text };
}

/**
 * The caption text that should currently show, or null when nothing should.
 * Null when the viewer has switched the native track on (mode 'showing') so the
 * overlay defers to the browser's own rendering, and null when no cue is active
 * — including the gap after a cue ends, which is what clears the overlay on
 * loop without needing an 'ended' handler.
 *
 * @param {TextTrack} track
 * @returns {string | null}
 */
export function activeCaptionText(track) {
  if (track.mode === 'showing') return null;
  const cues = track.activeCues;
  const cue = cues && cues.length > 0 ? cues[0] : null;
  return cue ? /** @type {VTTCue} */ (cue).text : null;
}

/**
 * Wire an overlay to a demo video's caption track. `onChange` is invoked with
 * the caption text to display, or null to clear — on every event that can
 * change what should be on screen: cue boundaries, seeks, playback start, and
 * the viewer toggling native captions via the controls (a TextTrackList
 * 'change'), so the overlay↔native handoff is immediate rather than lagging a
 * cue. Pause is deliberately not wired: an active caption stays readable while
 * paused, and clears on its own when its cue ends.
 *
 * @param {HTMLVideoElement} video
 * @param {TextTrack} track
 * @param {(text: string | null) => void} onChange
 */
export function wireDemoCaption(video, track, onChange) {
  // Hidden mode still loads cues and fires cuechange, but suppresses the
  // browser's own caption rendering so only the styled overlay shows.
  track.mode = 'hidden';
  const update = () => onChange(activeCaptionText(track));
  track.addEventListener('cuechange', update);
  video.textTracks.addEventListener('change', update);
  video.addEventListener('seeked', update);
  video.addEventListener('play', update);
  update();
}
