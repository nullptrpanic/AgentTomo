FROM ubuntu:24.04

LABEL org.opencontainers.image.title="AgentTomo Playground" \
      org.opencontainers.image.description="Ubuntu with common network and process diagnostic tools"

RUN apt-get update -o Acquire::Retries=3 \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        dnsutils \
        file \
        git \
        iproute2 \
        iptables \
        iputils-ping \
        jq \
        less \
        lsof \
        nano \
        net-tools \
        netcat-openbsd \
        openssh-client \
        procps \
        psmisc \
        python3 \
        ripgrep \
        strace \
        tcpdump \
        traceroute \
        tree \
        unzip \
        vim-tiny \
        wget \
        xz-utils \
        zip \
    && rm -rf /var/lib/apt/lists/*

ENV LANG=C.UTF-8

CMD ["/bin/bash"]
