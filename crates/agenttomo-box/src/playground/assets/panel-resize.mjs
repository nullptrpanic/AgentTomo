// The terminal and remote desktop already observe their available dimensions.
export function resizePanels(layout, handle) {
  const narrow = matchMedia("(max-width: 900px)");
  let share = 0.55, pointer = null, grab = 0;
  const available = () => layout.clientWidth - handle.offsetWidth;
  const clamp = width => Math.max(320, Math.min(available() - 360, width));
  function render() {
    if (narrow.matches) return;
    const width = clamp(available() * share);
    layout.style.setProperty("--workspace-width", `${width}px`);
    handle.setAttribute("aria-valuenow", String(Math.round(width / available() * 100)));
    handle.setAttribute("aria-valuemin", String(Math.round(320 / available() * 100)));
    handle.setAttribute("aria-valuemax", String(Math.round((available() - 360) / available() * 100)));
  }
  function move(width) {
    share = clamp(width) / available();
    render();
  }
  function stop() {
    pointer = null;
    layout.classList.remove("resizing-panels");
  }
  handle.addEventListener("pointerdown", event => {
    if (event.button !== 0 || narrow.matches) return;
    event.preventDefault();
    grab = event.clientX - handle.getBoundingClientRect().left;
    pointer = event.pointerId;
    handle.setPointerCapture(pointer);
    handle.focus({ preventScroll: true });
    layout.classList.add("resizing-panels");
  });
  handle.addEventListener("pointermove", event => {
    if (event.pointerId === pointer) move(event.clientX - layout.getBoundingClientRect().left - layout.clientLeft - grab);
  });
  handle.addEventListener("pointerup", event => {
    if (event.pointerId !== pointer) return;
    handle.releasePointerCapture(pointer);
    stop();
  });
  handle.addEventListener("pointercancel", stop);
  handle.addEventListener("lostpointercapture", stop);
  handle.addEventListener("keydown", event => {
    if (narrow.matches) return;
    const width = handle.getBoundingClientRect().left - layout.getBoundingClientRect().left - layout.clientLeft;
    const next = { ArrowLeft: width - 20, ArrowRight: width + 20, Home: 320, End: available() - 360 }[event.key];
    if (next === undefined) return;
    event.preventDefault();
    move(next);
  });
  handle.addEventListener("dblclick", () => { share = 0.55; render(); });
  new ResizeObserver(render).observe(layout);
  narrow.addEventListener("change", () => { stop(); render(); });
  render();
}
