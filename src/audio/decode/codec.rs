//! The codec registry: every decoder Symphonia ships plus Opus through libopus (adapted from
//! Psysonic's `psysonic-audio/src/codec.rs`).

use std::sync::OnceLock;

use symphonia::core::codecs::registry::CodecRegistry;

pub fn registry() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_audio_decoder::<symphonia_adapter_libopus::OpusDecoder>();
        registry
    })
}
