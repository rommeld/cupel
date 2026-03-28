fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .out_dir("src/generated")
        .compile_protos(&["proto/cellar.proto"], &["/proto"])?;
    Ok(())
}
