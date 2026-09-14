// Explicit feedback beside a write action. A reply confirms a save; a click does not.
export function createMutationFeedback({ status, button = null, pending = 'Saving…', success = 'Saved', slowAfter = 2000 }) {
  let flight = null, timer = null, destroyed = false;
  status.classList?.add('mutation-feedback');
  status.setAttribute('role', 'status');
  status.setAttribute('aria-live', 'polite');
  const show = (text, state) => {
    if (destroyed) return;
    status.textContent = text;
    status.dataset.state = state;
    status.hidden = !text;
  };
  show('', 'idle');
  function run(task) {
    if (flight) return flight;
    const label = button?.textContent, disabled = button?.disabled;
    show(pending, 'pending');
    if (button) { button.disabled = true; button.textContent = pending; button.setAttribute('aria-busy', 'true'); }
    timer = setTimeout(() => show('Still saving… Your changes are waiting for confirmation.', 'pending'), slowAfter);
    flight = Promise.resolve().then(task).then(value => {
      show(success, 'success');
      return value;
    }, error => {
      const uncertain = ['timeout', 'offline', 'disconnected'].includes(error?.code) || /timed out|timeout/i.test(error?.message || '');
      show(`${uncertain ? 'Save not confirmed' : 'Could not save'} — ${error?.message || 'Please try again.'}`, 'error');
      throw error;
    }).finally(() => {
      clearTimeout(timer); timer = null; flight = null;
      if (button && !destroyed) { button.disabled = disabled; button.textContent = label; button.removeAttribute('aria-busy'); }
    });
    return flight;
  }
  return { run, get pending() { return !!flight; }, clear() { if (!flight) show('', 'idle'); }, destroy() { destroyed = true; clearTimeout(timer); } };
}
