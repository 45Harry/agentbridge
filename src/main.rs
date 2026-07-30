fn main() {
    let registry = agentbridge::connectors::all();
    let detected: Vec<&str> = registry.detected().map(|c| c.id()).collect();
    println!("agentbridge {} — connectors registered: {}", env!("CARGO_PKG_VERSION"), registry.all().len());
    println!("detected on this machine: {:?}", detected);
}
