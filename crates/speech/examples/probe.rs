//! Spike 2 probe: which speech backends can Prism reach on this machine,
//! and does an utterance actually go out? Prints backend facts to stdout,
//! then speaks two short phrases (the second interrupts the first).
//!
//! Run with: cargo run -p speech --example probe --features prism

#[cfg(feature = "prism")]
fn main() {
    use speech::Speaker;
    let known = speech::PrismSpeaker::known_backends();
    println!("prism knows these backends: {}", known.join(", "));
    match speech::PrismSpeaker::connect() {
        Some(mut speaker) => {
            println!("active backend: {}", speaker.backend_name());
            println!("saying a short phrase...");
            speaker.speak("AsAccess speech probe. Prism is working.", false);
            std::thread::sleep(std::time::Duration::from_millis(2500));
            speaker.speak("Second phrase, interrupting.", true);
            std::thread::sleep(std::time::Duration::from_millis(1500));
            speaker.stop();
            println!("probe done");
        }
        None => {
            println!("no prism backend available");
            std::process::exit(1);
        }
    }
}

#[cfg(not(feature = "prism"))]
fn main() {
    println!("built without the prism feature");
}
