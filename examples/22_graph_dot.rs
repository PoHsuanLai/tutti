//! Build a small graph and print its Graphviz summary via `graph.dot()`.
//!
//! Run with: `cargo run -p tutti --example 22_graph_dot`
//!
//! Pipe the output to Graphviz to render an SVG:
//!   `cargo run -p tutti --example 22_graph_dot | dot -Tsvg > graph.svg`

use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().outputs(2).build()?;

    // Chain: sine -> lowpass -> master.
    let osc = engine.graph.add(sine_hz::<f32>(440.0));
    let filter = engine.graph.add(lowpass_hz::<f32>(2000.0, 1.0));
    engine.graph.pipe_all(osc, filter);
    engine.graph.pipe_output(filter);

    engine.graph.commit();

    println!("// tutti graph dot dump");
    println!("// nodes: {}", engine.graph.len());
    for id in engine.graph.ids() {
        println!(
            "//   node {:?}: {} inputs, {} outputs",
            id,
            engine.graph.inputs(id),
            engine.graph.outputs(id),
        );
    }
    println!();
    println!("{}", engine.graph.dot());

    Ok(())
}
