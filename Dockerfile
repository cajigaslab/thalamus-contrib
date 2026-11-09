# 26.04 for glibc 2.43: the thalamus-neuro wheel is manylinux_2_43.
FROM ubuntu:26.04
ARG DEBIAN_FRONTEND=noninteractive

# Qt's runtime libraries, plus the runtime versions of the -dev packages
# Thalamus' prepare.py builds native against (libva, libvdpau, libdrm, ...).
RUN apt-get update && apt-get install -y \
      python3 python3-venv xvfb imagemagick tmux \
      libgl1 libegl1 libopengl0 libfontconfig1 libfreetype6 libdbus-1-3 libglib2.0-0t64 \
      libxkbcommon0 libxkbcommon-x11-0 libxrender1 libxi6 libxext6 libsm6 libice6 \
      libxcb-cursor0 libxcb-icccm4 libxcb-image0 libxcb-keysyms1 libxcb-randr0 \
      libxcb-render-util0 libxcb-shape0 libxcb-xinerama0 libxcb-xfixes0 libxcb-xkb1 \
      libpulse0 libpipewire-0.3-0t64 libasound2t64 libnss3 \
      mesa-vulkan-drivers libvulkan1 \
      libudev1 libzstd1 libbz2-1.0 libdecor-0-0 libxss1 libxtst6 libxrandr2 \
      libwayland-client0 libwayland-cursor0 libwayland-egl1 libxinerama1 libxcursor1 \
      libgles2 libcurl4t64 libx11-6 libx11-xcb1 libxfixes3 libdrm2 libxcb1 libxcb-glx0 \
      libxcb-shm0 libxcb-sync1 libxcb-util1 libxcb-xinput0 libffi8 libkrb5-3 \
      libva2 libva-drm2 libva-x11-2 libvdpau1 libxv1 liblzma5 libpcre2-8-0 libbrotli1 \
      libusb-1.0-0 \
      clang cmake nasm pkg-config libssl-dev \
      libdbus-1-dev libasound2-dev libfreetype-dev libfontconfig-dev

# Qwen Code, which needs Node.js 20+ (Ubuntu 24.04's nodejs is 18), so Node
# comes from NodeSource. git and ripgrep are for the agent's own use.
RUN apt-get install -y curl ca-certificates git ripgrep \
 && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
 && apt-get install -y nodejs \
 && npm install -g @qwen-code/qwen-code \
 && rm -rf /var/lib/apt/lists/*

# Start the virtual display, then run whatever command was given (bash by default).
RUN printf '#!/bin/sh\nXvfb :99 -screen 0 1920x1080x24 >/tmp/xvfb.log 2>&1 &\nexec "$@"\n' > /entrypoint.sh \
 && chmod +x /entrypoint.sh

USER ubuntu
# thalamus-neuro comes from a locally built wheel in the build context.
COPY thalamus_neuro-1.0.64-py3-none-manylinux_2_43_x86_64.whl /tmp/
RUN python3 -m venv /home/ubuntu/venv \
 && /home/ubuntu/venv/bin/pip install \
      /tmp/thalamus_neuro-1.0.64-py3-none-manylinux_2_43_x86_64.whl hatch
# The venv is active everywhere: on PATH with VIRTUAL_ENV set for every
# process (docker exec, scripts), and activated by .bashrc in interactive
# shells, which also gives the (venv) prompt.
ENV VIRTUAL_ENV=/home/ubuntu/venv PATH=/home/ubuntu/venv/bin:$PATH DISPLAY=:99 XDG_RUNTIME_DIR=/tmp/runtime-thalamus
RUN echo 'source /home/ubuntu/venv/bin/activate' >> /home/ubuntu/.bashrc

# Rust via rustup, for ubuntu: Ubuntu 24.04's cargo (1.75) is too old for
# edition 2024 crates.
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy,rustfmt
ENV PATH=/home/ubuntu/.cargo/bin:$PATH

# Thalamus' user settings (thalamus/usersettings.py): crash report uploads
# already declined, and the missing .NET runtime dialog suppressed, so neither
# prompts on startup.
RUN mkdir -p /home/ubuntu/.thalamus \
 && printf '{"crashpad_consent_requested": true, "crashpad_consent_answer": false, "dotnet_runtime_warning_suppressed": true}\n' \
      > /home/ubuntu/.thalamus/settings.json

# For a Windows checkout bind-mounted at ~/thalamus-contrib: Docker Desktop
# shows it as owned by root, which git refuses to work in for any other user,
# so trust it; and it has CRLF line endings (core.autocrlf=true on Windows),
# so match that there, or every CRLF file shows as modified. Only there:
# clones made inside the container keep LF.
RUN git config --global --add safe.directory /home/ubuntu/thalamus-contrib \
 && git config --file /home/ubuntu/.gitconfig-windows core.autocrlf true \
 && git config --global includeIf.gitdir:/home/ubuntu/thalamus-contrib/.path /home/ubuntu/.gitconfig-windows

# Mount point for a named volume (-v <volume>:/home/ubuntu/work). It exists
# in the image, owned by ubuntu, so a new volume mounted here starts out owned
# by ubuntu too, rather than by root.
RUN mkdir -p /home/ubuntu/work

WORKDIR /home/ubuntu
ENTRYPOINT ["/entrypoint.sh"]
CMD ["bash"]

