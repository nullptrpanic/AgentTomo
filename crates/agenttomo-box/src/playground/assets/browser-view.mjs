// Display transport only. Chrome is launched by the traced container session.
export class BrowserView {
  constructor(screen, status, reconnect) {
    this.screen = screen;
    this.status = status;
    this.reconnect = reconnect;
    this.generation = 0;
    this.active = false;
    this.visible = false;
    reconnect.addEventListener("click", () => this.connect());
  }

  start(session, token) {
    this.url = new URL(`/api/sessions/${session}/desktop`, location.href);
    this.url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
    this.url.searchParams.set("token", token);
    this.active = true;
    if (this.visible) this.connect();
  }

  show(visible) {
    this.visible = visible;
    if (this.rfb) {
      this.rfb.viewOnly = !visible;
      if (!visible) this.rfb.blur();
    } else if (visible && this.active) {
      this.connect();
    }
  }

  async connect() {
    if (!this.active) return;
    const generation = ++this.generation;
    this.rfb?.disconnect();
    this.rfb = null;
    this.reconnect.disabled = true;
    this.status.textContent = "Connecting display…";
    try {
      const { default: RFB } = await import("./vendor/novnc.mjs");
      if (!this.active || generation !== this.generation) return;
      const rfb = new RFB(this.screen, this.url.href);
      this.rfb = rfb;
      rfb.scaleViewport = true;
      rfb.viewOnly = !this.visible;
      rfb.background = "#182029";
      rfb.addEventListener("connect", () => {
        if (this.rfb !== rfb) return;
        this.status.textContent = "Connected · Chrome";
        this.reconnect.disabled = false;
      });
      rfb.addEventListener("disconnect", () => {
        if (this.rfb !== rfb) return;
        this.rfb = null;
        this.status.textContent = this.active ? "Display disconnected · reconnect to continue" : "Session ended";
        this.reconnect.disabled = !this.active;
      });
    } catch (error) {
      if (generation !== this.generation) return;
      this.status.textContent = `Display unavailable: ${error.message}`;
      this.reconnect.disabled = !this.active;
    }
  }

  stop() {
    this.active = false;
    ++this.generation;
    this.rfb?.disconnect();
    this.rfb = null;
    this.reconnect.disabled = true;
    this.status.textContent = "Session ended";
  }
}
