fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("protos/statsig_forward_proxy.proto")?;
    Ok(())
}
