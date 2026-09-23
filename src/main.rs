mod device;
mod protocol;
mod settings;
mod worker;

fn main() {
    std::process::exit(device::qobject::igm5000_run());
}
