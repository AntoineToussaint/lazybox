/**
 * The sign-in guard in the page: while the agent acts, a form holding a filled
 * password field is not submitted until the person allows it, however the
 * submission comes about (a click, Enter, Space, NumpadEnter, a script calling
 * form.submit() or requestSubmit()). It is an init script, so it runs in every
 * frame before the page's own scripts. It is plain JavaScript in a string: a
 * bundler would wrap a function's inner helpers in names the page does not
 * have.
 *
 * The service arms it at the start of each agent action (arm(true, approved),
 * approved when the person already allowed this submission) and disarms it
 * after. An armed guard holds the submission, calls the exposed binding
 * `signInHeldBinding`, and waits for release() (the person allowed it) or
 * drop() (they did not). The binding is asynchronous, so the service also
 * reads pending() once the action's input is handled: a submission the action
 * made at once is never missed for arriving late.
 */

/** The binding the guard calls when it holds a submission. */
export const signInHeldBinding = '__lazyboxSignInHeld';

const key = "Symbol.for('lazybox.signInGuard')";

/** Whether a field holds a password: type password, or an autocomplete that says so (a shown password). */
export const secretFieldSelector = 'input[type=password i], input[autocomplete*=password i]';

export const signInGuardScript = `(() => {
  const key = ${key};
  if (window[key]) return;
  let armed = false, approved = false, releasing = 0;
  const held = [];
  const elements = Object.getOwnPropertyDescriptor(HTMLFormElement.prototype, 'elements').get;
  const holdsSecret = form => Array.prototype.some.call(elements.call(form), el =>
    el instanceof HTMLInputElement && el.value !== '' && el.matches(${JSON.stringify(secretFieldSelector)}));
  const guarded = form => armed && !approved && !releasing && holdsSecret(form);
  const hold = resubmit => {
    held.push(resubmit);
    try { window[${JSON.stringify(signInHeldBinding)}](); } catch (error) { /* the service is gone: nothing is released */ }
  };
  addEventListener('submit', event => {
    const form = event.target;
    if (!(form instanceof HTMLFormElement) || !guarded(form)) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    const submitter = event.submitter && event.submitter.form === form ? event.submitter : null;
    hold(() => form.requestSubmit(submitter));
  }, true);
  const submit = HTMLFormElement.prototype.submit;
  HTMLFormElement.prototype.submit = function () {
    if (!guarded(this)) return submit.call(this);
    const form = this;
    hold(() => submit.call(form));
  };
  Object.defineProperty(window, key, { value: Object.freeze({
    arm(on, ok) { armed = on; approved = ok; },
    release() {
      const all = held.splice(0);
      releasing++;
      try { for (const resubmit of all) resubmit(); } finally { releasing--; }
      return all.length;
    },
    drop() { return held.splice(0).length; },
    pending() { return held.length; },
  }) });
})();`;

/** Expressions the service evaluates in a frame. */
export const armExpression = (on: boolean, approved: boolean) => `window[${key}]?.arm(${on}, ${approved})`;
export const releaseExpression = `window[${key}]?.release()`;
export const dropExpression = `window[${key}]?.drop()`;
export const pendingExpression = `window[${key}]?.pending() || 0`;
