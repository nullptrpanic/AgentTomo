FROM agenttomo-playground:24.04

LABEL org.opencontainers.image.title="AgentTomo Playground with Chrome"

RUN apt-get update -o Acquire::Retries=3 \
    && curl --fail --location --retry 3 --output /tmp/chrome.deb \
       https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
       /tmp/chrome.deb xvfb x11vnc openbox xauth x11-utils libnss3-tools fonts-noto-cjk \
    && rm -f /tmp/chrome.deb \
    && rm -rf /var/lib/apt/lists/*

COPY --chmod=755 agenttomo-desktop agenttomo-chrome agenttomo-browser-session /usr/local/bin/
RUN ln -s agenttomo-chrome /usr/local/bin/google-chrome

ENV DISPLAY=:99 XAUTHORITY=/tmp/agenttomo-desktop/Xauthority
