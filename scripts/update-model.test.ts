import { test } from 'node:test';
import assert from 'node:assert/strict';
import { agentsNotice, automaticCheckDue, lastCheckedText, CHECK_EVERY_MS, compareVersions, isNewer, offerState, postUpdateDecision, progressFraction, progressText, readUpdatePreferences, releasesSince, remindValue, REMIND_LATER_MS, updateErrorText, UPDATE_PREFERENCES, updatePath, versionAgreement } from '../packages/surfaces/src/update-model.ts';

const update = (version: string, extra = {}) => ({ version, current: '0.1.0', notes: '', date: '2026-12-01 10:00:00.0 +00:00:00', ...extra });
const prefs = (values: Record<string, string> = {}) => readUpdatePreferences(name => values[name]);
const NOW = Date.parse('2026-12-02T09:00:00Z');

test('versions compare as semantic versions, prereleases before their release', () => {
  assert.ok(compareVersions('0.2.0', '0.1.9') > 0);
  assert.ok(compareVersions('0.10.0', '0.9.0') > 0, 'numbers, not strings');
  assert.ok(compareVersions('1.0.0', '0.99.99') > 0);
  assert.equal(compareVersions('0.2.0', 'v0.2.0'), 0);
  assert.equal(compareVersions('0.2.0+build.7', '0.2.0'), 0, 'build metadata does not order');
  assert.ok(compareVersions('0.2.0-rc.1', '0.2.0') < 0);
  assert.ok(compareVersions('0.2.0-rc.2', '0.2.0-rc.10') < 0, 'numeric identifiers compare as numbers');
  assert.ok(compareVersions('0.2.0-alpha', '0.2.0-beta') < 0);
  assert.ok(compareVersions('0.2.0-1', '0.2.0-alpha') < 0, 'numeric before alphanumeric');
  assert.ok(compareVersions('0.2.0-rc', '0.2.0-rc.1') < 0, 'fewer identifiers first');
  // Something that is not a version (a dated Dev release id) is never newer.
  assert.equal(isNewer('2026-11-17.5', '0.1.0'), false);
  assert.equal(isNewer('0.1.0', '2026-11-17.5'), true);
  assert.equal(isNewer('0.1.0', '0.1.0'), false);
});

test('an update is offered until skipped or put off, and a skip is for one version only', () => {
  assert.equal(offerState(undefined, prefs(), NOW), 'none');
  assert.equal(offerState(update('0.2.0'), prefs(), NOW), 'offer');
  assert.equal(offerState(update('0.2.0'), prefs({ [UPDATE_PREFERENCES.skipped]: '0.2.0' }), NOW), 'skipped');
  // A later version after a skip is offered again.
  assert.equal(offerState(update('0.2.1'), prefs({ [UPDATE_PREFERENCES.skipped]: '0.2.0' }), NOW), 'offer');
  // Remind me later: quiet for a day, for that version.
  const later = prefs({ [UPDATE_PREFERENCES.remind]: remindValue('0.2.0', NOW) });
  assert.deepEqual(later.remind, { version: '0.2.0', until: NOW + REMIND_LATER_MS });
  assert.equal(offerState(update('0.2.0'), later, NOW + 60_000), 'snoozed');
  assert.equal(offerState(update('0.2.0'), later, NOW + REMIND_LATER_MS + 1), 'offer');
  assert.equal(offerState(update('0.2.1'), later, NOW + 60_000), 'offer', 'a newer version is not put off with the old one');
  // A skip outranks a reminder.
  assert.equal(offerState(update('0.2.0'), prefs({ [UPDATE_PREFERENCES.skipped]: '0.2.0', [UPDATE_PREFERENCES.remind]: remindValue('0.2.0', NOW) }), NOW + REMIND_LATER_MS * 2), 'skipped');
  // Garbage in the store reads as nothing chosen.
  assert.equal(prefs({ [UPDATE_PREFERENCES.remind]: 'nonsense' }).remind, undefined);
  assert.equal(prefs({ [UPDATE_PREFERENCES.lastChecked]: 'x' }).lastChecked, 0);
});

test('automatic checks are on by default, every six hours across launches, and can be turned off', () => {
  assert.equal(prefs().automatic, true);
  assert.equal(automaticCheckDue(prefs(), NOW), true, 'never checked: check now');
  const checked = (at: number, extra = {}) => prefs({ [UPDATE_PREFERENCES.lastChecked]: String(at), ...extra });
  assert.equal(automaticCheckDue(checked(NOW - 60_000), NOW), false);
  assert.equal(automaticCheckDue(checked(NOW - CHECK_EVERY_MS), NOW), true);
  assert.equal(automaticCheckDue(checked(NOW + 3_600_000), NOW), true, 'a clock that went back does not stop checks');
  assert.equal(automaticCheckDue(checked(0, { [UPDATE_PREFERENCES.automatic]: 'off' }), NOW), false);
  assert.equal(prefs({ [UPDATE_PREFERENCES.automatic]: 'on' }).automatic, true);
});

test('what is new is everything between the installed version and the update', () => {
  const releases = [
    { id: '0.3.0', date: '2026-12-10', title: 'Third', items: [{ title: 'C', body: 'c' }] },
    { id: '2026-12-05.1', date: '2026-12-05', title: 'Between', items: [{ title: 'B2', body: 'b2' }] },
    { id: '0.2.0', date: '2026-12-01', title: 'Second', items: [{ title: 'B', body: 'b' }] },
    { id: '0.1.0', date: '2026-11-17', title: 'First', items: [{ title: 'A', body: 'a' }] },
  ];
  assert.deepEqual(releasesSince(update('0.3.0', { releases }), '0.1.0').releases.map(r => r.id), ['0.3.0', '2026-12-05.1', '0.2.0']);
  assert.equal(releasesSince(update('0.3.0', { releases }), '0.1.0').complete, true);
  assert.deepEqual(releasesSince(update('0.3.0', { releases }), '0.2.0').releases.map(r => r.id), ['0.3.0', '2026-12-05.1']);
  // Older than every section the feed carries: all of them, marked partial.
  const old = releasesSince(update('0.3.0', { releases }), '0.0.1');
  assert.equal(old.releases.length, 4);
  assert.equal(old.complete, false);
  // A feed with plain notes only (an older build script): one section from its lines.
  const plain = releasesSince(update('0.2.0', { notes: 'Second\n• New thing: It works.\n• Other: Also.' }), '0.1.0');
  assert.deepEqual(plain.releases[0], { id: '0.2.0', date: '2026-12-01', title: 'Second', items: [{ title: 'New thing', body: 'It works.' }, { title: 'Other', body: 'Also.' }] });
  assert.deepEqual(releasesSince(update('0.2.0'), '0.1.0').releases, []);
});

test('the install warning counts running agents and says they keep running', () => {
  assert.equal(agentsNotice([]), undefined);
  assert.equal(agentsNotice([{ closed: true }, { historical: true }]), undefined, 'ended conversations are not running agents');
  const one = agentsNotice([{ busy: true }, { closed: true }])!;
  assert.equal(one.count, 1);
  assert.equal(one.title, '1 agent is running, in the middle of a turn. It keeps running.');
  const three = agentsNotice([{ busy: true }, {}, {}])!;
  assert.equal(three.title, '3 agents are running, 1 in the middle of a turn. They keep running.');
  assert.equal(agentsNotice([{}, {}])!.title, '2 agents are running. They keep running.');
  assert.equal(agentsNotice([{ busy: true }, { busy: true }])!.title, '2 agents are running, all in the middle of a turn. They keep running.');
  assert.match(three.body, /restarts its Runner/);
  assert.match(three.body, /approval that is waiting stays waiting/);
});

test('failures and progress read as sentences', () => {
  assert.match(updateErrorText('offline', 'check'), /could not reach GitHub/);
  assert.match(updateErrorText('offline', 'install'), /Nothing was changed/);
  assert.match(updateErrorText('signature', 'install'), /not installed\. Your current version is untouched/);
  assert.match(updateErrorText('permission', 'install'), /Applications/);
  assert.match(updateErrorText('feed', 'check'), /v2-updates/);
  assert.equal(updateErrorText(undefined, 'install', 'disk full'), 'The update could not be installed: disk full. Your current version keeps working.');
  assert.equal(progressText({ phase: 'download', downloaded: 50, total: 200 }), 'Downloading… 25%');
  assert.equal(progressText({ phase: 'download', downloaded: 50, total: 0 }), 'Downloading…');
  assert.equal(progressText({ phase: 'runner', downloaded: 0, total: 0 }, '0.2.0'), 'Moving the Runner to 0.2.0…');
  assert.equal(progressText({ phase: 'verify', downloaded: 0, total: 0 }), 'Checking the signature…');
  assert.equal(progressFraction({ phase: 'download', downloaded: 0, total: 0 }), undefined);
  assert.ok(progressFraction({ phase: 'download', downloaded: 100, total: 100 })! < progressFraction({ phase: 'verify', downloaded: 0, total: 0 })!);
});

test('the launch after an install says whether it took', () => {
  assert.equal(postUpdateDecision('', '0.2.0'), 'none');
  assert.equal(postUpdateDecision(null, '0.2.0'), 'none');
  assert.equal(postUpdateDecision('0.2.0', '0.2.0'), 'updated');
  assert.equal(postUpdateDecision('0.2.0', '0.1.0'), 'failed');
  assert.equal(postUpdateDecision('garbage', '0.1.0'), 'none');
});

test('app and Runner versions must agree in a release, and a mismatch says what to do', () => {
  assert.deepEqual(versionAgreement({ release: false, app: '0.0.0', runner: 'dev' }), { agree: true, problem: '', canMove: false });
  assert.equal(versionAgreement({ release: true, app: '0.2.0', runner: '0.2.0' }).agree, true);
  assert.deepEqual(versionAgreement({ release: true, ownRunner: false, app: '0.2.0', runner: '0.1.0' }), { agree: true, problem: '', canMove: false });
  const behind = versionAgreement({ release: true, app: '0.2.0', runner: '0.1.0' });
  assert.equal(behind.agree, false);
  assert.equal(behind.canMove, true);
  assert.match(behind.problem, /still 0\.1\.0 and the app is 0\.2\.0/);
  assert.match(behind.problem, /Agents keep running/);
  assert.match(versionAgreement({ release: true, app: '0.1.0', runner: '0.2.0' }).problem, /newer than this app/);
  assert.equal(versionAgreement({ release: true, app: '0.2.0', runner: '' }).canMove, false);
});

test('the last check reads as a time a person understands', () => {
  const now = Date.parse('2026-12-02T15:00:00');
  assert.equal(lastCheckedText(0, now), 'Never checked');
  assert.equal(lastCheckedText(now - 10_000, now), 'Checked just now');
  assert.equal(lastCheckedText(now - 5 * 60_000, now), 'Checked 5 minutes ago');
  assert.equal(lastCheckedText(now - 60_000, now), 'Checked 1 minute ago');
  assert.match(lastCheckedText(now - 3 * 3_600_000, now), /^Checked today at /);
  assert.match(lastCheckedText(now - 24 * 3_600_000, now), /^Checked yesterday at /);
});

test('a build from source says how it updates, and never that it checks a feed', () => {
  const source = updatePath({ channel: 'release', updater: false, source: { commit: '0123456789abcdef0123', dirty: false, checkout: '/Users/me/lazybox' } });
  assert.equal(source.kind, 'source');
  assert.match(source.text, /^Built from source; update with make update in \/Users\/me\/lazybox\./);
  assert.equal(source.commit, '0123456789ab');
  assert.equal(updatePath({ channel: 'release', updater: false, source: { commit: 'abc', dirty: true, checkout: '' } }).commit, 'abc, with uncommitted changes');
  // Even a source build that somehow carried an updater updates with make update.
  assert.equal(updatePath({ channel: 'release', updater: true, source: { commit: '', dirty: false, checkout: '' } }).kind, 'source');
  assert.equal(updatePath({ channel: 'release', updater: true, source: null }).kind, 'updater');
  assert.equal(updatePath({ channel: 'ai.lazybox.v2.working', updater: false }).kind, 'development');
  assert.equal(updatePath(undefined).kind, 'browser');
});
