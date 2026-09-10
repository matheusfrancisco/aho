fn main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = world_ns::linux::helper_main() {
        eprintln!("aho-ns-helper: {error}");
        std::process::exit(125);
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("aho-ns-helper: Linux namespaces are unavailable on this platform; no command was executed.");
        std::process::exit(125);
    }
}
