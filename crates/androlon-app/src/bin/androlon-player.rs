//! androlon-player — the suite's single-app player. This is the binary an
//! appified `.app` bundle executes (its `CFBundleExecutable` symlinks here):
//! one Coherence pane with input, audio, and keymap, nothing else. The
//! package comes from `--app <pkg>` or `ANDROLON_APP` (bundles configure via
//! `LSEnvironment` — Launch Services passes no custom argv).

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let package = args
        .iter()
        .position(|a| a == "--app")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var("ANDROLON_APP").ok());

    match package {
        Some(pkg) => androlon_app::app::run_single(&pkg),
        None => {
            eprintln!("androlon-player: no app given (--app <package> or ANDROLON_APP)");
            std::process::exit(2);
        }
    }
}
