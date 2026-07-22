/**
 * Keeps transient review dialogs usable with a keyboard.  Native <dialog>
 * support differs between WebViews, so the small action deliberately owns
 * initial focus, Tab looping, Escape, and focus restoration.
 */
export interface FocusTrapOptions {
  onClose: () => void;
}

const selector = [
  '[data-dialog-initial-focus]',
  'button:not([disabled])',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[href]',
  '[tabindex]:not([tabindex="-1"])'
].join(',');

function focusable(node: HTMLElement) {
  return [...node.querySelectorAll<HTMLElement>(selector)]
    .filter((element) => !element.hasAttribute('hidden') && element.getAttribute('aria-hidden') !== 'true');
}

export function focusTrap(node: HTMLElement, options: FocusTrapOptions) {
  const previous = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
  let current = options;

  // A microtask lets Svelte finish inserting conditional form fields before
  // we pick the first meaningful target, without waiting for a paint (which
  // is inconsistent in embedded WebViews and test DOMs).
  queueMicrotask(() => {
    const target = node.querySelector<HTMLElement>('[data-dialog-initial-focus]') ?? focusable(node)[0] ?? node;
    target.focus({ preventScroll: true });
  });

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      current.onClose();
      return;
    }
    if (event.key !== 'Tab') return;
    const items = focusable(node);
    if (!items.length) {
      event.preventDefault();
      node.focus();
      return;
    }
    const first = items[0];
    const last = items.at(-1)!;
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  node.addEventListener('keydown', onKeydown);
  return {
    update(next: FocusTrapOptions) { current = next; },
    destroy() {
      node.removeEventListener('keydown', onKeydown);
      // Returning focus is important after a palette/modal closes, but only
      // if its launcher is still in the live document.
      if (previous?.isConnected) previous.focus({ preventScroll: true });
    }
  };
}
