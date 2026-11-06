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
