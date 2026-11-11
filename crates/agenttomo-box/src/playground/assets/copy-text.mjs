import { checkIcon } from "./icons.mjs";

const feedback = new WeakMap();
export const resetCopyFeedback = button => feedback.get(button)?.();

export async function copyWithFeedback(button, text) {
  resetCopyFeedback(button);
  const icon = button.innerHTML, label = button.getAttribute("aria-label"), title = button.title;
  let timer;
  const reset = () => {
    if (feedback.get(button) !== reset) return;
    clearTimeout(timer);
    button.innerHTML = icon; button.title = title;
    if (label === null) button.removeAttribute("aria-label"); else button.setAttribute("aria-label", label);
    button.removeEventListener("mouseleave", reset);
    feedback.delete(button);
  };
  feedback.set(button, reset);
  button.addEventListener("mouseleave", reset);
  try { await copyText(text); } catch (error) { reset(); throw error; }
  if (!button.isConnected) { reset(); return; }
  if (feedback.get(button) !== reset) return;
  button.innerHTML = checkIcon; button.title = "Copied"; button.setAttribute("aria-label", "Copied");
  timer = setTimeout(reset, 2000);
}

// Clipboard API requires HTTPS. The development UI also supports plain HTTP.
export async function copyText(text) {
  if (globalThis.isSecureContext && navigator.clipboard?.writeText) {
    try { await navigator.clipboard.writeText(text); return; } catch { /* Try browser copy below. */ }
  }
  const focused = document.activeElement;
  const selection = document.getSelection();
  const ranges = [];
  if (selection) for (let i = 0; i < selection.rangeCount; i++) ranges.push(selection.getRangeAt(i).cloneRange());
  const field = document.createElement("textarea");
  field.value = text;
  field.readOnly = true;
  field.setAttribute("aria-label", "Clipboard copy");
  Object.assign(field.style, { position: "fixed", left: "0", top: "0", opacity: "0", pointerEvents: "none" });
  document.body.append(field);
  try {
    field.focus({ preventScroll: true });
    field.select();
    if (!document.execCommand("copy")) throw new Error("Clipboard unavailable. Select the text and copy it manually.");
  } finally {
    field.remove();
    focused?.focus({ preventScroll: true });
    if (selection) { selection.removeAllRanges(); for (const range of ranges) selection.addRange(range); }
  }
}
