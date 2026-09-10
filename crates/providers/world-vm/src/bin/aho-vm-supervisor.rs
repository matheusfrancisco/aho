fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 2 {
        eprintln!("usage: aho-vm-supervisor <private-launch.json>");
        std::process::exit(2);
    }
    if let Err(error) = world_vm::supervise(std::path::Path::new(&args[1])) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
