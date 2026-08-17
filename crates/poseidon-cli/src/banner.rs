//! The startup banner - the POSEIDON emblem + a random motto, printed to stderr
//! when the CLI is invoked interactively. Skipped in `--json` mode so machine
//! output on stdout stays clean; stderr keeps it clear of stdout regardless.

use std::time::{SystemTime, UNIX_EPOCH};

/// Emblem width, for centring the wordmark + motto beneath it.
const WIDTH: usize = 40;

/// The POSEIDON emblem - a trident over waves.
const LOGO: &str = r#"
%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%
%%%%%%%%%%%%%%%%%%%#++%%%%%%%%%%%%%%%%%%%%
%%%%%%%%%%%%%%%%%%#+==+%%%%%%%%%%%%%%%%%%%
%%%%%%%%%%%%%%%%%#+====+%%%%%%%%%%%%%%%%%%
%%%%%%##%%%%%%%%#+======*%%%%%%%%%##%%%%%%
%%%%%%#=+#%%%%%%*++====++#%%%%%%#++#%%%%%%
%%%%%%#+==+#%%%%%%*====*%%%%%%#+==+%%%%%%%
%%%%%%%+====+#%%%%*====*%%%%#+====*%%%%%%%
%%%%%%%*====+#%%%%*====*%%%%*=====*%%%%%%%
%%%%%%%*====+#%%%%*====*%%%%#+====*%%%%%%%
%%%%%%%#====+#%%%%*====*%%%%#+====#%%%%%%%
%%%%%%%#====+#%%%%*====*%%%%#+====#%%%%%%%
%%%%%%%#====+#%%%%*====*%%%%#+===+#%%%%%%%
%%%%%%%#+===+#%%%%*====*%%%%#+===+#%%%%%%%
%%%%%%%#+========================+#%%%%%%%
%%%%%%%#+========================+#%%%%%%%
%%%%%%%%%%%#%%%%%%#+=========++++*%%%%%%%*
%%%%#*++========+*#%%#***#%%%%%%%%%%%%%#++
%#*=================+*#%%%%%%%%%%%%%%*==+#
+==*#########*+==========+*####**+=====*%%
%%%%%%%%%%%%%%%%%#+==================+%%%%
%%%%%%%%%%%%%%%%%%%%%*+==========+*#%%%%%%
%%%%%%%%%%%%%%%%%%%%%%%%%%####%%%%%%%%%%%%"#;

/// Print the emblem, wordmark, and a random motto to stderr.
pub fn print() {
    let motds = poseidon_core::MOTDS;
    let motto = motds[pick(motds.len())];
    eprintln!("{LOGO}");
    eprintln!("{}", centre("P O S E I D E N"));
    eprintln!("{}", centre(&format!("\"{motto}\"")));
    eprintln!();
}

/// Centre `s` within the emblem width.
fn centre(s: &str) -> String {
    let pad = WIDTH.saturating_sub(s.chars().count()) / 2;
    format!("{}{s}", " ".repeat(pad))
}

/// A cheap random index from the clock's sub-second nanos - no `rand` dependency
/// for a purely cosmetic pick.
fn pick(len: usize) -> usize {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    nanos % len.max(1)
}
