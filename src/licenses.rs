//! What has to be shown wherever the program goes.

/// libopus is built in, and its licence asks for its notice to come with the binary.
pub const TEXT: &str = concat!(
    "mesh-kvm includes libopus, under this licence:\n\n",
    include_str!("../third_party/opus/COPYING"),
    "\nmesh-kvm includes the Inter and JetBrains Mono fonts, under this licence:\n\n",
    include_str!("../third_party/fonts/Inter-OFL.txt"),
    "\n",
    include_str!("../third_party/fonts/JetBrainsMono-OFL.txt"),
    "\nmesh-kvm is also built from Rust crates, each under its own licence, mostly MIT or Apache 2.0. ",
    "Cargo.lock in the source lists them, and each crate's source has its licence.\n",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_opus_notice_is_there_in_full() {
        assert!(TEXT.contains("Redistribution and use in source and binary forms"));
        assert!(TEXT.contains("Xiph.Org"));
        assert!(TEXT.contains("THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS"));
    }

    #[test]
    fn both_font_licences_are_there_in_full() {
        assert_eq!(TEXT.matches("SIL OPEN FONT LICENSE Version 1.1").count(), 2);
        assert!(TEXT.contains("Inter"));
        assert!(TEXT.contains("JetBrains Mono"));
    }
}
