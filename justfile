name   := "cosmic-voice"
app_id := "dev.techgeek1.CosmicExtAppletVoice"
prefix := "/usr"

build:
    cargo build --release

install: build
    install -Dm0755 target/release/{{name}} {{prefix}}/bin/{{name}}
    install -Dm0644 data/{{app_id}}.desktop {{prefix}}/share/applications/{{app_id}}.desktop
    install -Dm0644 data/icons/hicolor/scalable/apps/cosmic-voice.svg {{prefix}}/share/icons/hicolor/scalable/apps/{{app_id}}.svg

uninstall:
    rm -f {{prefix}}/bin/{{name}}
    rm -f {{prefix}}/share/applications/{{app_id}}.desktop
    rm -f {{prefix}}/share/icons/hicolor/scalable/apps/{{app_id}}.svg

base     := "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models"
offline  := "sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming"
online   := "sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11"

# Directory names the default config expects.
offline_dir := "sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8"
online_dir  := "sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8"

# Fetch both resident ASR models into the user data dir, under the names the
# default config expects.
models:
    mkdir -p ~/.local/share/cosmic-voice/models
    cd ~/.local/share/cosmic-voice/models && \
        for m in {{offline}} {{online}}; do \
            curl -LO {{base}}/$m.tar.bz2 && tar xf $m.tar.bz2 && rm $m.tar.bz2; \
        done && \
        rm -rf {{offline_dir}} {{online_dir}} && \
        mv {{offline}} {{offline_dir}} && \
        mv {{online}} {{online_dir}}

run:
    RUST_LOG=cosmic_voice=debug cargo run
