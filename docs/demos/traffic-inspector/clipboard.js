// Standalone demo counterpart of the Playground's clipboard fallback.
export async function copyText(text) {
  if (globalThis.isSecureContext && navigator.clipboard?.writeText) {
    try { await navigator.clipboard.writeText(text); return; } catch { /* HTTP/browser fallback. */ }
  }
  const focused = document.activeElement;
  const field = document.createElement("textarea");
  field.value = text;
  field.readOnly = true;
  Object.assign(field.style, { position: "fixed", left: "0", top: "0", opacity: "0" });
  document.body.append(field);
  try {
    field.focus({ preventScroll: true });
    field.select();
    if (!document.execCommand("copy")) throw new Error("复制失败，请选中文本后手动复制。");
  } finally { field.remove(); focused?.focus({ preventScroll: true }); }
}
