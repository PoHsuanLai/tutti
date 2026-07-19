//! Show display method output.
use fundsp_tutti::prelude64::*;

fn main() {
    let mut node = lowpass_hz(1000.0, 0.5);

    print!("Filter: lowpass_hz(1000.0, 0.5)\n\n");

    // The display method prints an ASCII oscilloscope and other information about the node.
    print!("{}", node.display());
}
