import assert from 'node:assert/strict';
import test from 'node:test';

import { activeCaptionText, captionParts, wireDemoCaption } from '../src/scripts/demo-captions.js';

// Fake media element + caption track built on real EventTargets, so the tests
// exercise the wiring by dispatching the same events a browser would.
function harness() {
  const track = new EventTarget();
  track.mode = 'disabled';
  track.activeCues = null;
  const textTracks = new EventTarget();
  const video = new EventTarget();
  video.textTracks = textTracks;
  const calls = [];
  wireDemoCaption(video, track, (text) => calls.push(text));
  return { track, textTracks, video, calls };
}

const setCue = (track, text) => {
  track.activeCues = text === null ? [] : [{ text }];
};

test('captionParts splits a keystroke cue into chip and description', () => {
  assert.deepEqual(captionParts('⌨ g m — merge the focused pull request'), {
    key: 'g m',
    text: 'merge the focused pull request',
  });
});

test('captionParts leaves a context cue whole even when it holds an em dash', () => {
  // No leading keyboard glyph, so the em dash is content, not a chord separator.
  assert.deepEqual(captionParts('One inbox for every repo — PRs, issues and CI'), {
    key: null,
    text: 'One inbox for every repo — PRs, issues and CI',
  });
});

test('activeCaptionText yields the active cue, or null when nothing should show', () => {
  const track = { mode: 'hidden', activeCues: null };
  assert.equal(activeCaptionText(track), null, 'no cue list yet');
  track.activeCues = [];
  assert.equal(activeCaptionText(track), null, 'cue ended — the gap clears the overlay');
  track.activeCues = [{ text: '⌨ ! — jump to the agent that needs your input' }];
  assert.equal(activeCaptionText(track), '⌨ ! — jump to the agent that needs your input');
  track.mode = 'showing';
  assert.equal(activeCaptionText(track), null, 'native captions on — defer, do not double up');
});

test('wiring hides the native track and emits an initial empty state', () => {
  const { track, calls } = harness();
  assert.equal(track.mode, 'hidden', 'native rendering is suppressed for the overlay');
  assert.deepEqual(calls, [null], 'no cue active yet, so the overlay starts cleared');
});

test('a cue boundary drives the overlay', () => {
  const { track, calls } = harness();
  setCue(track, '⌨ ]]s — open the snippet picker');
  track.dispatchEvent(new Event('cuechange'));
  assert.equal(calls.at(-1), '⌨ ]]s — open the snippet picker');
});

test('pausing keeps the active caption on screen', () => {
  const { track, video, calls } = harness();
  setCue(track, '⌨ a x — start Codex on this task');
  track.dispatchEvent(new Event('cuechange'));
  const before = calls.length;
  video.dispatchEvent(new Event('pause'));
  assert.equal(calls.length, before, 'pause must not re-render or clear the caption');
  assert.equal(calls.at(-1), '⌨ a x — start Codex on this task');
});

test('toggling native captions hands off immediately, without waiting for a cue', () => {
  const { track, textTracks, calls } = harness();
  setCue(track, '⌨ g — the GitHub menu');
  track.dispatchEvent(new Event('cuechange'));
  assert.equal(calls.at(-1), '⌨ g — the GitHub menu');

  // Viewer enables captions in the controls: the list fires 'change'.
  track.mode = 'showing';
  textTracks.dispatchEvent(new Event('change'));
  assert.equal(calls.at(-1), null, 'overlay defers the moment native captions turn on');

  // Viewer turns them back off: overlay reclaims the still-active cue at once.
  track.mode = 'hidden';
  textTracks.dispatchEvent(new Event('change'));
  assert.equal(calls.at(-1), '⌨ g — the GitHub menu');
});

test('a cue ending clears the overlay on its own, so no ended handler is needed', () => {
  const { track, video, calls } = harness();
  setCue(track, '⌨ ]]s — open the snippet picker');
  track.dispatchEvent(new Event('cuechange'));
  setCue(track, null);
  track.dispatchEvent(new Event('cuechange'));
  assert.equal(calls.at(-1), null, 'the post-cue gap clears the overlay');

  const before = calls.length;
  video.dispatchEvent(new Event('ended'));
  assert.equal(calls.length, before, 'ended is not wired; looping clears via cuechange');
});
