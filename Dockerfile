# Clean-room build and test for copycat:  docker build -t copycat:test .
# The suite runs during the build, so a broken commit fails the image. Nothing
# runs the resulting image; CI's docker-debian job only builds it.

FROM rust:1.96-bookworm

# The list from .github/workflows/ci.yml's build-test job. Keep them in sync.
# libxdo is the one people miss: tray-icon -> muda -> libxdo-sys links -lxdo.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
        libxkbcommon-dev \
        libgtk-3-dev libayatana-appindicator3-dev \
        libgl1-mesa-dev libegl1-mesa-dev \
        libx11-dev libxrandr-dev libxi-dev libxcursor-dev libwayland-dev \
        libxdo-dev \
        xvfb xauth \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /copycat
COPY . .

RUN cargo build --workspace --locked
# xvfb-run gives the GUI-linked copycat-ui test binary a display.
RUN xvfb-run -a cargo test --workspace --locked
