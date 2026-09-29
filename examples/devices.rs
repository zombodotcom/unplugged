//! Lists audio devices as cpal sees them: `cargo run --example devices`
use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    for id in cpal::available_hosts() {
        println!("== {} ==", id.name());
        let host = cpal::host_from_id(id).unwrap();
        for d in host.devices().unwrap() {
            println!("  {d}");
            match d.default_input_config() {
                Ok(c) => println!("    in : {c:?}"),
                Err(e) => println!("    in : ({e})"),
            }
            match d.default_output_config() {
                Ok(c) => println!("    out: {c:?}"),
                Err(e) => println!("    out: ({e})"),
            }
        }
    }
}
