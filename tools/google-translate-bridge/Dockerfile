# The bridge in a container: Node 22 and Chrome (Google Chrome on amd64; Google has no Linux build for arm64, so Chromium there), with the
# fonts and the time zone of a Japanese desktop. The Chrome of a Linux container is not the Chrome of a Mac: see "Advanced" in the README.
#
#   docker build -t google-translate-bridge tools/google-translate-bridge
#   docker run -d --name bridge --restart unless-stopped --memory 1500m -p 127.0.0.1:8130:8130 \
#     -e BRIDGE_TOKEN=secret -v bridge-profile:/data google-translate-bridge
FROM node:22-bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl procps tini tzdata xvfb xauth fonts-noto-cjk fonts-noto-color-emoji fonts-liberation \
 && if [ "$(dpkg --print-architecture)" = amd64 ]; then \
      curl -fsSL -o /tmp/chrome.deb https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb \
      && apt-get install -y --no-install-recommends /tmp/chrome.deb && rm /tmp/chrome.deb; \
    else \
      apt-get install -y --no-install-recommends chromium; \
    fi \
 && ln -s "$(command -v google-chrome-stable || command -v chromium)" /usr/local/bin/chrome \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /data && chown node:node /data

# `--no-sandbox`: the container is the sandbox (Chrome's own needs privileges that a container does not have).
# `HEADLESS=0` in a virtual screen (xvfb): the token of a headless Chrome on Linux is often not honoured by Google (the answer is then the
# classic one: 13 of 36 texts), the one of a Chrome with a window almost always is (1 of 36).
ENV TZ=Asia/Tokyo \
    HEADLESS=0 \
    LANG=C.UTF-8 \
    CHROME=/usr/local/bin/chrome \
    CHROME_ARGS=--no-sandbox \
    HOST=0.0.0.0 \
    PORT=8130 \
    PROFILE=/data/profile

WORKDIR /app
COPY server.mjs .
USER node
VOLUME /data
EXPOSE 8130

ENTRYPOINT ["tini", "--"]
CMD ["xvfb-run", "-a", "-s", "-screen 0 1920x1080x24", "node", "server.mjs"]
