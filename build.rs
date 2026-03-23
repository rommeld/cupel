// build.rs runs BEFORE your src/ code compiles.
//
// It calls the protobuf compiler (protoc) on your .proto file and writes
// generated Rust code into a temp directory (OUT_DIR).
// You then pull that code into your project with tonic::include_proto!()
//
// Rule to remember: any time you change metrics.proto, run `cargo build`
// to regenerate. The compiler won't warn you — old generated code will
// silently stay in place.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/metrics.proto")?;
    Ok(())
}
